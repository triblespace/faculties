//! Field table seed for Claude Code transcripts (`projects/*/*.jsonl`).
//!
//! This seed covers the `user` and `assistant` records of an ordinary turn:
//! prompts, text, tool calls, and tool results with text, image,
//! `tool_reference` and `document` items. Every other record kind and field
//! is unknown and fails, as the contract requires. The rest of the table
//! (attachments, system records, request logs, thinking per model) is added
//! with the v3 Claude Code importer.

use super::{Class, Cond, Decode, DropReason, Entry, FieldTable, Order, Structure};

const ENVELOPE: DropReason =
    DropReason::Machinery("client envelope: working directory, branch, client version");
const STOP_REASONS: &[(&str, &str)] = &[
    ("end_turn", "end_turn"),
    ("tool_use", "tool_use"),
    ("stop_sequence", "stop_sequence"),
    ("max_tokens", "max_tokens"),
    ("refusal", "refusal"),
];
const IS_ERROR: &[(&str, &str)] = &[("true", "error"), ("false", "ok")];

macro_rules! record {
    ($kind:literal) => {
        [
            Entry::new(
                concat!("{", $kind, "}.type"),
                Class::Structure(Structure::Discriminator),
            ),
            Entry::new(
                concat!("{", $kind, "}.uuid"),
                Class::Structure(Structure::Order),
            ),
            Entry::new(
                concat!("{", $kind, "}.parentUuid"),
                Class::Structure(Structure::Order),
            ),
            Entry::new(
                concat!("{", $kind, "}.sessionId"),
                Class::Annotation(&[("source_locator", Decode::Text)]),
            ),
            Entry::new(
                concat!("{", $kind, "}.timestamp"),
                Class::Annotation(&[("timestamp", Decode::Timestamp)]),
            ),
            Entry::new(concat!("{", $kind, "}.cwd"), Class::Drop(ENVELOPE)),
            Entry::new(concat!("{", $kind, "}.gitBranch"), Class::Drop(ENVELOPE)),
            Entry::new(concat!("{", $kind, "}.version"), Class::Drop(ENVELOPE)),
            Entry::new(concat!("{", $kind, "}.userType"), Class::Drop(ENVELOPE)),
            Entry::new(concat!("{", $kind, "}.entrypoint"), Class::Drop(ENVELOPE)),
            Entry::new(
                concat!("{", $kind, "}.isSidechain"),
                Class::Drop(DropReason::Machinery(
                    "subagent flag; the subagent link is structure from meta.json",
                )),
            ),
        ]
    };
}

/// A `tool_result` item list shared by every place Claude Code nests one.
macro_rules! result_items {
    ($prefix:literal) => {
        [
            Entry::new(
                concat!($prefix, "{text}.type"),
                Class::Structure(Structure::Discriminator),
            ),
            Entry::fact(
                concat!($prefix, "{text}.text"),
                "text",
                "in",
                "payload",
                Decode::Text,
            ),
            Entry::new(
                concat!($prefix, "{image}.type"),
                Class::Structure(Structure::Discriminator),
            ),
            Entry::new(
                concat!($prefix, "{image}.source{base64}.type"),
                Class::Structure(Structure::Discriminator),
            ),
            Entry::fact(
                concat!($prefix, "{image}.source{base64}.media_type"),
                "image",
                "in",
                "media_type",
                Decode::MediaType,
            ),
            Entry::fact(
                concat!($prefix, "{image}.source{base64}.data"),
                "image",
                "in",
                "blob",
                Decode::Base64,
            ),
            Entry::new(
                concat!($prefix, "{tool_reference}.type"),
                Class::Structure(Structure::Discriminator),
            ),
            // A tool_reference loads the named tool's definition into her
            // window where it stands.
            Entry::fact(
                concat!($prefix, "{tool_reference}.tool_name"),
                "tool-definition",
                "in",
                "tool_name",
                Decode::Text,
            ),
            Entry::new(
                concat!($prefix, "{document}.type"),
                Class::Structure(Structure::Discriminator),
            ),
            Entry::new(
                concat!($prefix, "{document}.source{base64}.type"),
                Class::Structure(Structure::Discriminator),
            ),
            Entry::fact(
                concat!($prefix, "{document}.source{base64}.media_type"),
                "file",
                "in",
                "media_type",
                Decode::MediaType,
            ),
            Entry::fact(
                concat!($prefix, "{document}.source{base64}.data"),
                "file",
                "in",
                "blob",
                Decode::Base64,
            ),
        ]
    };
}

const USER: [Entry; 11] = record!("user");
const ASSISTANT: [Entry; 11] = record!("assistant");
const RESULT_ITEMS: [Entry; 12] = result_items!("{user}.message.content[]{tool_result}.content[]");

const USER_MESSAGE: &[Entry] = &[
    Entry::new(
        "{user}.message.role",
        Class::Annotation(&[("raw_role", Decode::Text)]),
    ),
    Entry::fact(
        "{user}.message.content",
        "text",
        "in",
        "payload",
        Decode::Text,
    ),
    Entry::new(
        "{user}.message.content[]{text}.type",
        Class::Structure(Structure::Discriminator),
    ),
    Entry::fact(
        "{user}.message.content[]{text}.text",
        "text",
        "in",
        "payload",
        Decode::Text,
    ),
    Entry::new(
        "{user}.message.content[]{image}.type",
        Class::Structure(Structure::Discriminator),
    ),
    Entry::new(
        "{user}.message.content[]{image}.source{base64}.type",
        Class::Structure(Structure::Discriminator),
    ),
    Entry::fact(
        "{user}.message.content[]{image}.source{base64}.media_type",
        "image",
        "in",
        "media_type",
        Decode::MediaType,
    ),
    Entry::fact(
        "{user}.message.content[]{image}.source{base64}.data",
        "image",
        "in",
        "blob",
        Decode::Base64,
    ),
    Entry::new(
        "{user}.message.content[]{tool_result}.type",
        Class::Structure(Structure::Discriminator),
    ),
    Entry::new(
        "{user}.message.content[]{tool_result}.tool_use_id",
        Class::Correlator,
    ),
    Entry::fact(
        "{user}.message.content[]{tool_result}.is_error",
        "tool-result",
        "in",
        "status",
        Decode::Term(IS_ERROR),
    ),
    Entry::fact(
        "{user}.message.content[]{tool_result}.content",
        "text",
        "in",
        "payload",
        Decode::Text,
    ),
    Entry::new("{user}.sourceToolAssistantUUID", Class::Correlator),
    Entry::new("{user}.promptId", Class::Drop(DropReason::ProviderId)),
    Entry::new(
        "{user}.toolUseResult",
        Class::Drop(DropReason::Machinery(
            "client-side originals and structured copies of a tool result; \
             she saw the tool_result content",
        )),
    ),
];

const ASSISTANT_MESSAGE: &[Entry] = &[
    Entry::new("{assistant}.requestId", Class::Drop(DropReason::ProviderId)),
    Entry::new(
        "{assistant}.message{message}.type",
        Class::Structure(Structure::Discriminator),
    ),
    Entry::new("{assistant}.message{message}.id", Class::Correlator),
    Entry::new(
        "{assistant}.message{message}.role",
        Class::Annotation(&[("raw_role", Decode::Text)]),
    ),
    Entry::new(
        "{assistant}.message{message}.model",
        Class::Annotation(&[("raw_model", Decode::Text), ("generated_by", Decode::Text)]),
    )
    .when(&[Cond::IsNot("message.model", "<synthetic>")]),
    Entry::new(
        "{assistant}.message{message}.stop_reason",
        Class::Annotation(&[("ends", Decode::Term(STOP_REASONS))]),
    )
    .when(&[Cond::IsNot("message.stop_reason", "null")]),
    Entry::new(
        "{assistant}.message{message}.stop_reason",
        Class::Drop(DropReason::Empty),
    )
    .when(&[Cond::Is("message.stop_reason", "null")]),
    Entry::new(
        "{assistant}.message{message}.stop_sequence",
        Class::Drop(DropReason::Empty),
    ),
    Entry::new(
        "{assistant}.message{message}.usage",
        Class::Drop(DropReason::Machinery("token accounting")),
    ),
    Entry::new(
        "{assistant}.message{message}.content[]{text}.type",
        Class::Structure(Structure::Discriminator),
    ),
    Entry::fact(
        "{assistant}.message{message}.content[]{text}.text",
        "text",
        "out",
        "payload",
        Decode::Text,
    ),
    Entry::new(
        "{assistant}.message{message}.content[]{tool_use}.type",
        Class::Structure(Structure::Discriminator),
    ),
    Entry::new(
        "{assistant}.message{message}.content[]{tool_use}.id",
        Class::Correlator,
    ),
    Entry::fact(
        "{assistant}.message{message}.content[]{tool_use}.name",
        "tool-call",
        "out",
        "tool_name",
        Decode::Text,
    ),
    Entry::fact(
        "{assistant}.message{message}.content[]{tool_use}.input",
        "tool-call",
        "out",
        "structure",
        Decode::Json,
    ),
    Entry::new(
        "{assistant}.message{message}.content[]{tool_use}.caller",
        Class::Open("direct versus programmatic caller: whether its result entered her window"),
    ),
    Entry::new(
        "{assistant}.message{message}.content[]{thinking}.type",
        Class::Structure(Structure::Discriminator),
    ),
    Entry::new(
        "{assistant}.message{message}.content[]{thinking}.thinking",
        Class::Open("readable thinking: her own or a summarizer's, per model (gloss table)"),
    ),
    Entry::new(
        "{assistant}.message{message}.content[]{thinking}.signature",
        Class::Open("sealed thinking bytes: their media type is a format constant"),
    ),
];

const fn concat_entries<const N: usize>(parts: &[&[Entry]]) -> [Entry; N] {
    let mut out = [Entry::new("", Class::Correlator); N];
    let mut at = 0;
    let mut part = 0;
    while part < parts.len() {
        let mut i = 0;
        while i < parts[part].len() {
            out[at] = parts[part][i];
            at += 1;
            i += 1;
        }
        part += 1;
    }
    assert!(at == N, "entry count");
    out
}

const LEN: usize = USER.len()
    + ASSISTANT.len()
    + RESULT_ITEMS.len()
    + USER_MESSAGE.len()
    + ASSISTANT_MESSAGE.len();
const ENTRIES: [Entry; LEN] = concat_entries(&[
    &USER,
    &ASSISTANT,
    &RESULT_ITEMS,
    USER_MESSAGE,
    ASSISTANT_MESSAGE,
]);

/// The Claude Code transcript field table (seed).
pub const TABLE: FieldTable = FieldTable {
    jsonl: true,
    format: "claude-code",
    discriminators: &["type"],
    id_keyed: &[],
    record: "",
    order: Order::Parents {
        id: "uuid",
        parent: "parentUuid",
    },
    entries: &ENTRIES,
};
