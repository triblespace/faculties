//! Field table seed for ChatGPT data exports (`conversations*.json`, G1).
//!
//! A unit is one conversation; a record is one node of its `mapping`. This
//! seed covers text and code content of system, user, assistant and tool
//! messages, the recipient she addressed, and the mapping's links. Every
//! other field is unknown and fails. The rest of the table (the ~190 named
//! metadata keys, attachments, multimodal parts) is added with the v3
//! ChatGPT importer.

use super::{
    Beside, Class, Cond, Decode, DropReason, Entry, FieldTable, Gap, GapRule, Gaps, Mirror, Order,
    Structure, Windows,
};

const ROLE: &str = "message.author.role";
const RECIPIENT: &str = "message.recipient";
const NODE: &str = "[].mapping.<id>";

macro_rules! node {
    ($tail:literal) => {
        concat!("[].mapping.<id>", $tail)
    };
}

const TO_TOOL: &[Cond] = &[Cond::Is(ROLE, "assistant"), Cond::IsNot(RECIPIENT, "all")];
const TO_ALL: &[Cond] = &[Cond::Is(ROLE, "assistant"), Cond::Is(RECIPIENT, "all")];

const ENTRIES: &[Entry] = &[
    // The conversation.
    Entry::new(
        "[].id",
        Class::Annotation(&[("source_locator", Decode::Text)]),
    ),
    Entry::new(
        "[].conversation_id",
        Class::Drop(DropReason::Mirror(Mirror::Sibling("id"))),
    ),
    // A titler wrote the title about her conversation, not she.
    Entry::new(
        "[].title",
        Class::Annotation(&[("gloss:title", Decode::Text)]),
    ),
    Entry::new(
        "[].create_time",
        Class::Drop(DropReason::Machinery("the export's conversation times")),
    ),
    Entry::new(
        "[].update_time",
        Class::Drop(DropReason::Machinery("the export's conversation times")),
    ),
    Entry::new("[].current_node", Class::Drop(DropReason::ProviderId)),
    // The mapping's links.
    Entry::new(node!(".id"), Class::Structure(Structure::Order)),
    Entry::new(node!(".parent"), Class::Structure(Structure::Order)),
    Entry::new(
        node!(".children"),
        Class::Drop(DropReason::Mirror(Mirror::Children)),
    ),
    Entry::new(
        node!(".message"),
        Class::Structure(Structure::Shape(
            "a mapping node without a message holds nothing",
        )),
    ),
    // The message.
    Entry::new(node!(".message.id"), Class::Correlator),
    Entry::new(
        node!(".message.author.role"),
        Class::Annotation(&[("raw_role", Decode::Text)]),
    ),
    Entry::new(
        node!(".message.author.name"),
        Class::Open(
            "whether ChatGPT rendered a tool's name in its reply (Harmony does; \
             ChatGPT unverified)",
        ),
    )
    .when(&[
        Cond::Is(ROLE, "tool"),
        Cond::IsNot("message.author.name", "null"),
    ]),
    Entry::new(
        node!(".message.author.name"),
        Class::Drop(DropReason::Empty),
    )
    .when(&[Cond::Is("message.author.name", "null")]),
    Entry::new(
        node!(".message.author.metadata"),
        Class::Drop(DropReason::Empty),
    ),
    Entry::new(
        node!(".message.create_time"),
        Class::Annotation(&[("timestamp", Decode::UnixSeconds)]),
    )
    .when(&[Cond::IsNot("message.create_time", "null")]),
    Entry::new(
        node!(".message.create_time"),
        Class::Drop(DropReason::Empty),
    )
    .when(&[Cond::Is("message.create_time", "null")]),
    Entry::new(
        node!(".message.update_time"),
        Class::Drop(DropReason::Machinery("edit time of the export record")),
    ),
    // She addressed a tool: its name is part of her call.
    Entry::fact(
        node!(".message.recipient"),
        "tool-call",
        "out",
        "tool_name",
        Decode::Text,
    )
    .when(TO_TOOL),
    // Addressed to the conversation.
    Entry::new(
        node!(".message.recipient"),
        Class::Structure(Structure::Vocabulary(&["all"])),
    )
    .when(&[Cond::Is(RECIPIENT, "all")]),
    Entry::new(
        node!(".message.content{text}.content_type"),
        Class::Structure(Structure::Discriminator),
    ),
    Entry::fact(
        node!(".message.content{text}.parts[]"),
        "text",
        "in",
        "payload",
        Decode::Text,
    )
    .when(&[Cond::Is(ROLE, "user")]),
    Entry::fact(
        node!(".message.content{text}.parts[]"),
        "text",
        "out",
        "payload",
        Decode::Text,
    )
    .when(TO_ALL),
    Entry::fact(
        node!(".message.content{text}.parts[]"),
        "text",
        "in",
        "payload",
        Decode::Text,
    )
    .when(&[Cond::Is(ROLE, "tool")]),
    Entry::fact(
        node!(".message.content{text}.parts[]"),
        "system",
        "in",
        "payload",
        Decode::Text,
    )
    .when(&[Cond::Is(ROLE, "system")]),
    Entry::new(
        node!(".message.content{code}.content_type"),
        Class::Structure(Structure::Discriminator),
    ),
    // The raw input of a call she addressed to a tool.
    Entry::fact(
        node!(".message.content{code}.text"),
        "tool-call",
        "out",
        "payload",
        Decode::Text,
    )
    .when(TO_TOOL),
    Entry::new(
        node!(".message.metadata.is_visually_hidden_from_conversation"),
        Class::Drop(DropReason::Machinery("UI visibility; she was shown it")),
    ),
];

/// The ChatGPT export field table (seed).
pub const TABLE: FieldTable = FieldTable {
    jsonl: false,
    format: "chatgpt",
    namespace: "chatgpt",
    discriminators: &["content_type"],
    id_keyed: &["[].mapping"],
    record: NODE,
    order: Order::Parents {
        id: "id",
        parent: "parent",
    },
    windows: Windows::NONE,
    gaps: Gaps {
        beside: &[
            // A tool message is a result: its head holds no value.
            GapRule {
                path: node!(".message.author.role"),
                when: &[Cond::Is(ROLE, "tool")],
                at: Beside::Before,
                gap: Gap {
                    modality: "tool-result",
                    direction: "in",
                    kind: "",
                },
                held: true,
            },
        ],
        ..Gaps::NONE
    },
    links: &[],
    places: &[],
    entries: ENTRIES,
};
