//! Resident Events tools. Names and text are literal; no tool opens a
//! caller-selected file, reads stdin, or chooses the server's local calendar day.
use super::{presentation, AddOptions, CalendarInput, Events as Operations};
use crate::mcp::{decode_arguments, invalid_arguments, Faculty, Tool};
use crate::out::Out;
use anybytes::Bytes;
use anyhow::{bail, Result};
use serde::Deserialize;
use std::path::PathBuf;

pub struct Events {
    operations: Operations,
}
impl Events {
    pub fn new(pile: PathBuf, key: Option<PathBuf>) -> Self {
        Self::with_storage(crate::storage::Storage::new(pile, key))
    }
    pub fn with_storage(storage: crate::storage::Storage) -> Self {
        Self {
            operations: Operations::with_storage(storage),
        }
    }
}
const TOOLS: &[Tool] = &[
    Tool {
        name: "events_add",
        description: "Add an event and optional initial note in one publication. ISO dates and timezone-free datetimes are interpreted as UTC; timezone-bearing RFC3339 is respected. Missing end means one day for a date or one hour otherwise. All prose, including @ strings, is literal.",
        input_schema: r#"{"type":"object","properties":{"summary":{"type":"string"},"from":{"type":"string"},"to":{"type":"string"},"rrule":{"type":"string"},"location":{"type":"string"},"status":{"type":"string","enum":["tentative","confirmed","cancelled"]},"transp":{"type":"string","enum":["opaque","transparent"]},"description":{"type":"string"},"note":{"type":"string"}},"required":["summary","from"],"additionalProperties":false}"#,
    },
    Tool {
        name: "events_list",
        description: "List occurrences overlapping an explicit ISO window. Dates and timezone-free datetimes mean UTC; no host-local today/week assumption. Preserves the existing bounded recurrence projection (up to 10000 generated instances per event) and cancellation filtering.",
        input_schema: r#"{"type":"object","properties":{"from":{"type":"string"},"to":{"type":"string"},"include_cancelled":{"type":"boolean","default":false}},"required":["from","to"],"additionalProperties":false}"#,
    },
    Tool {
        name: "events_next",
        description: "Find the next noncancelled occurrence using existing RRULE/RDATE/EXDATE treatment, through the 2100-01-01 horizon. after is ISO/UTC; omit it to capture the current time once.",
        input_schema: r#"{"type":"object","properties":{"after":{"type":"string"}},"additionalProperties":false}"#,
    },
    Tool {
        name: "events_note",
        description: "Attach an immutable literal note to an event ID or unambiguous prefix. @file and @- are ordinary text.",
        input_schema: r#"{"type":"object","properties":{"id":{"type":"string","minLength":1},"text":{"type":"string"}},"required":["id","text"],"additionalProperties":false}"#,
    },
    Tool {
        name: "events_show",
        description: "Show the selected event, its cancellation state, UID, description and notes from one frozen authorized observation.",
        input_schema: r#"{"type":"object","properties":{"id":{"type":"string","minLength":1}},"required":["id"],"additionalProperties":false}"#,
    },
    Tool {
        name: "events_cancel",
        description: "Idempotently assert monotone cancellation. Baseline event fields are not overwritten and exact repeats publish nothing.",
        input_schema: r#"{"type":"object","properties":{"id":{"type":"string","minLength":1}},"required":["id"],"additionalProperties":false}"#,
    },
    Tool {
        name: "events_resolve",
        description: "Resolve an event ID or unambiguous hexadecimal prefix.",
        input_schema: r#"{"type":"object","properties":{"prefix":{"type":"string","minLength":1}},"required":["prefix"],"additionalProperties":false}"#,
    },
    Tool {
        name: "events_ingest",
        description: "Ingest resident iCalendar text documents, staging the full batch before one publication. Names are diagnostics, not host paths. Exact duplicate definitions are skipped; a changed definition of a known UID (an edit, a move, a new EXDATE or override) is recorded as a new version beside the old one, never refused, and list/next use the current version per UID and RECURRENCE-ID by SEQUENCE, then LAST-MODIFIED. Nothing is removed unless complete_calendar is given: then the single document is recorded as a complete export of that calendar (a name the caller chooses and reuses for every export of it), dated by its latest DTSTAMP, and an event the newest complete export of its calendar no longer holds stops showing in list and next. An older export ingested later changes nothing. Never pass complete_calendar for a single invitation or a filtered export. Times: Z is UTC; TZID is applied (an IANA zone, or a fixed-offset VTIMEZONE in the document; anything else is an error naming the event), with a nonexistent local time moved forward by the DST gap and an ambiguous one taken as the first occurrence; RRULEs expand in the DTSTART zone, so local wall time holds across DST. Floating times (no Z, no TZID) are local times in floating_zone (an IANA name, e.g. Europe/Berlin); without it they are an error, never UTC. VALUE=DATE stays an all-day date at UTC midnight. EXDATE is applied; a RECURRENCE-ID override is its own event and replaces its master's instance. RDATE is not read.",
        input_schema: r#"{"type":"object","properties":{"documents":{"type":"array","minItems":1,"items":{"type":"object","properties":{"name":{"type":"string","minLength":1},"text":{"type":"string"}},"required":["name","text"],"additionalProperties":false}},"floating_zone":{"type":"string","minLength":1},"complete_calendar":{"type":"string","minLength":1}},"required":["documents"],"additionalProperties":false}"#,
    },
];
#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Status {
    Tentative,
    Confirmed,
    Cancelled,
}
#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Transparency {
    Opaque,
    Transparent,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Add {
    summary: String,
    from: String,
    to: Option<String>,
    rrule: Option<String>,
    location: Option<String>,
    status: Option<Status>,
    transp: Option<Transparency>,
    description: Option<String>,
    note: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct List {
    from: String,
    to: String,
    #[serde(default)]
    include_cancelled: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Next {
    after: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Note {
    id: String,
    text: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Event {
    id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Prefix {
    prefix: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Calendar {
    name: String,
    text: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Ingest {
    #[serde(deserialize_with = "crate::mcp::object::vec")]
    documents: Vec<Calendar>,
    #[serde(default)]
    floating_zone: Option<String>,
    #[serde(default)]
    complete_calendar: Option<String>,
}
fn selector(value: &str) -> Result<()> {
    let value = value.trim();
    if value.is_empty() || value.len() > 32 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid_arguments(
            "event selector must be a nonempty hexadecimal ID or prefix, at most 32 characters",
        ));
    }
    Ok(())
}
impl Faculty for Events {
    fn tools(&self) -> &[Tool] {
        TOOLS
    }
    fn call(&self, name: &str, arguments: Bytes, out: &mut Out<'_>) -> Result<()> {
        match name {
            "events_add" => {
                let args: Add = decode_arguments(arguments)?;
                let window = super::event_window(&args.from, args.to.as_deref())
                    .map_err(invalid_arguments)?;
                let mut options = AddOptions::new(args.summary, window);
                options.rrule = args.rrule;
                options.location = args.location;
                options.description = args.description;
                options.note = args.note;
                options.status = match args.status {
                    Some(Status::Tentative) => super::STATUS_TENTATIVE,
                    Some(Status::Cancelled) => super::STATUS_CANCELLED,
                    Some(Status::Confirmed) | None => super::STATUS_CONFIRMED,
                }
                .to_owned();
                options.transp = match args.transp {
                    Some(Transparency::Transparent) => super::TRANSP_TRANSPARENT,
                    Some(Transparency::Opaque) | None => super::TRANSP_OPAQUE,
                }
                .to_owned();
                options.validate().map_err(invalid_arguments)?;
                presentation::added(&self.operations.add(&options)?, out)
            }
            "events_list" => {
                let args: List = decode_arguments(arguments)?;
                let window =
                    super::event_window(&args.from, Some(&args.to)).map_err(invalid_arguments)?;
                let listed = self.operations.list(window, args.include_cancelled)?;
                out.notes(&listed.notes)?;
                presentation::occurrences(&listed, out)
            }
            "events_next" => {
                let args: Next = decode_arguments(arguments)?;
                let after = args
                    .after
                    .as_deref()
                    .map(super::parse_iso8601)
                    .transpose()
                    .map_err(invalid_arguments)?
                    .map(super::chrono_to_epoch);
                let next = self.operations.next(after)?;
                out.notes(&next.notes)?;
                presentation::occurrences(next.as_ref().map_or(&[], std::slice::from_ref), out)
            }
            "events_note" => {
                let args: Note = decode_arguments(arguments)?;
                selector(&args.id)?;
                presentation::noted(&self.operations.note(&args.id, &args.text)?, out)
            }
            "events_show" => {
                let args: Event = decode_arguments(arguments)?;
                selector(&args.id)?;
                presentation::show(&self.operations.show(&args.id)?, out)
            }
            "events_cancel" => {
                let args: Event = decode_arguments(arguments)?;
                selector(&args.id)?;
                presentation::cancelled(&self.operations.cancel(&args.id)?, out)
            }
            "events_resolve" => {
                let args: Prefix = decode_arguments(arguments)?;
                selector(&args.prefix)?;
                let resolved = self.operations.resolve(&args.prefix)?;
                out.notes(&resolved.notes)?;
                out.line(format!("{:x}", resolved.value))
            }
            "events_ingest" => {
                let args: Ingest = decode_arguments(arguments)?;
                let documents = args
                    .documents
                    .iter()
                    .map(|document| CalendarInput {
                        name: &document.name,
                        text: &document.text,
                    })
                    .collect::<Vec<_>>();
                super::validate_calendars(&documents).map_err(invalid_arguments)?;
                let options = super::IngestOptions {
                    floating_zone: args
                        .floating_zone
                        .as_deref()
                        .map(super::parse_floating_zone)
                        .transpose()
                        .map_err(invalid_arguments)?,
                    complete_calendar: args.complete_calendar,
                };
                presentation::ingested(&self.operations.ingest(&documents, options)?, out)
            }
            _ => bail!("unknown Events tool '{name}'"),
        }
    }
}
