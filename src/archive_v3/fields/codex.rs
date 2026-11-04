//! Field table for Codex rollouts (`~/.codex/sessions/**/rollout-*.jsonl`).
//!
//! It names every field path of the five format generations the 2026-11-04
//! census found (map-codex):
//!
//! - **G0** (2025-07): a `{id, timestamp}` header line, then bare response
//!   items (`{"type": "message", ...}`);
//! - **G1** (2025-08..10): the header adds `instructions` and `git`, and
//!   `record_type` lines appear;
//! - **G2** (2025-11 on): every line is a `{timestamp, type, payload}`
//!   envelope: `session_meta`, `turn_context`, `response_item`, `event_msg`,
//!   `compacted`;
//! - **G3** (2026-07 on): `world_state`, `inter_agent_communication_metadata`,
//!   the `agent_message` response item, `namespace` on calls;
//! - **G4** (2026-08 on): `ordinal` on every line, `item_completed` events,
//!   then `token_usage_record` and response-item `metadata`.
//!
//! What her window rendered: the response items, in file order. The system
//! section is `base_instructions` (a `system` fact), recorded from 2026-01;
//! before that it is a gap, and tool definitions are never recorded (a
//! `tool-definition` span gap). Each request opens with the settings header
//! it was sampled under (`turn_context.effort`, S1). A compaction installs
//! exactly its `replacement_history` (or, before that existed, its summary
//! after a span gap) as a new line.
//!
//! The event stream (`event_msg`) is the client's log of the same items:
//! every copy of an item is a mirror the unit must hold, every UI-only event
//! is named and dropped, and only `turn_aborted` and `task_complete.error`
//! add anything (why the turn ended, and the error). Rows marked
//! [`Class::Open`] are fields whose rendering is not verified; they fail the
//! unit and each says why.

use super::{
    Beside, Class, Cond, Decode, DropReason, Entry, FieldTable, FileWindows, Gap, GapRule, Gaps,
    Link, Mirror, Order, Place, Structure, Supply, Windows,
};
use crate::schemas::blockdag::content_fact::sealed;

/// The sealed form of an inter-agent message's content. The rollout names
/// the field `encrypted_content`; the media type is ours.
pub const OPENAI_ENCRYPTED_CONTENT: &str = "application/vnd.openai.encrypted-content";

const ENVELOPE: DropReason =
    DropReason::Machinery("client envelope: versions, paths, ids, origin of the session");
const UI: DropReason =
    DropReason::Machinery("a client UI event: what the call showed the user, never her window");
const USAGE: DropReason = DropReason::Machinery("token accounting");
const ORDINAL: DropReason = DropReason::Machinery("the line's position: order is the line");
const PASSTHROUGH: DropReason =
    DropReason::Machinery("client bookkeeping on an item: its turn and creation time");
const SETTING: DropReason = DropReason::Machinery(
    "a request setting the client renders, when it does, into a developer or user message, \
     which is kept",
);
const TIME: DropReason = DropReason::Machinery("the time a record that holds no item was written");
const STATUS: DropReason = DropReason::Machinery("the client's item status");

const DISCRIMINATOR: Class = Class::Structure(Structure::Discriminator);
const EFFORT: &[(&str, &str)] = &[
    ("low", "low"),
    ("medium", "medium"),
    ("high", "high"),
    ("xhigh", "xhigh"),
    ("max", "max"),
    ("ultra", "ultra"),
    ("minimal", "minimal"),
    ("none", "none"),
];
const DETAIL: &[(&str, &str)] = &[
    ("auto", "auto"),
    ("low", "low"),
    ("high", "high"),
    ("original", "original"),
];
const ABORTED: &[(&str, &str)] = &[("interrupted", "interrupted")];
const PHASES: &[&str] = &["commentary", "final_answer"];
const REGISTERS: &[(&str, &str)] = &[("commentary", "commentary"), ("final_answer", "answer")];

/// An `agent_message` event's phase, a copy of her message's (X1).
const PHASE_COPY: Supply = Supply {
    text: "payload.message",
    of: &[
        ("{message}{assistant}.content[]{output_text}.text", "phase"),
        (
            "{response_item}.payload{message}{assistant}.content[]{output_text}.text",
            "payload.phase",
        ),
    ],
    attribute: "register",
    decode: Decode::Term(REGISTERS),
    modality: "text",
    direction: "out",
};
const REASONING_SEAL: &[(&str, Decode)] =
    &[("media_type", Decode::ConstText(sealed::OPENAI_REASONING))];
const COMPACTION_SEAL: &[(&str, Decode)] =
    &[("media_type", Decode::ConstText(sealed::OPENAI_COMPACTION))];
const CONTENT_SEAL: &[(&str, Decode)] =
    &[("media_type", Decode::ConstText(OPENAI_ENCRYPTED_CONTENT))];
const DATA_URI_TYPE: &[(&str, Decode)] = &[("media_type", Decode::DataUriType)];
const COMMENTARY: &[(&str, Decode)] = &[("register", Decode::ConstTerm("commentary"))];
const ANSWER: &[(&str, Decode)] = &[("register", Decode::ConstTerm("answer"))];
const LOCAL_SHELL: &[(&str, Decode)] = &[("tool_name", Decode::ConstText("local_shell"))];
const WEB_SEARCH: &[(&str, Decode)] = &[("tool_name", Decode::ConstText("web_search"))];
const SUMMARY: Class = Class::Annotation(&[("gloss:reasoning-summary", Decode::Text)]);

/// The texts a client copy must be found in.
pub const USER_TEXTS: &[&str] = &[
    "{message}{user}.content[]{input_text}.text",
    "{response_item}.payload{message}{user}.content[]{input_text}.text",
    "{compacted}.payload.replacement_history[]{message}{user}.content[]{input_text}.text",
];
pub const INSTRUCTION_TEXTS: &[&str] = &[
    "{message}{user}.content[]{input_text}.text",
    "{response_item}.payload{message}{user}.content[]{input_text}.text",
    "{compacted}.payload.replacement_history[]{message}{user}.content[]{input_text}.text",
    "{message}{developer}.content[]{input_text}.text",
    "{response_item}.payload{message}{developer}.content[]{input_text}.text",
    "{compacted}.payload.replacement_history[]{message}{developer}.content[]{input_text}.text",
];
pub const ASSISTANT_TEXTS: &[&str] = &[
    "{message}{assistant}.content[]{output_text}.text",
    "{response_item}.payload{message}{assistant}.content[]{output_text}.text",
    "{compacted}.payload.replacement_history[]{message}{assistant}.content[]{output_text}.text",
];
pub const SUMMARY_TEXTS: &[&str] = &[
    "{reasoning}.summary[]{summary_text}.text",
    "{response_item}.payload{reasoning}.summary[]{summary_text}.text",
];
pub const IMAGE_URLS: &[&str] = &[
    "{message}{user}.content[]{input_image}.image_url",
    "{response_item}.payload{message}{user}.content[]{input_image}.image_url",
    "{compacted}.payload.replacement_history[]{message}{user}.content[]{input_image}.image_url",
];

/// A message she was shown, of `$modality`, at `$at`.
macro_rules! shown {
    ($at:expr, $modality:literal) => {
        [
            Entry::new(concat!($at, ".type"), DISCRIMINATOR),
            Entry::new(concat!($at, ".role"), DISCRIMINATOR),
            Entry::new(concat!($at, ".id"), Class::Drop(DropReason::ProviderId)),
            Entry::new(
                concat!($at, ".internal_chat_message_metadata_passthrough"),
                Class::Drop(PASSTHROUGH),
            ),
            Entry::new(concat!($at, ".content"), Class::Drop(DropReason::Empty)),
            Entry::new(concat!($at, ".content[]{input_text}.type"), DISCRIMINATOR),
            Entry::fact(
                concat!($at, ".content[]{input_text}.text"),
                $modality,
                "in",
                "payload",
                Decode::Text,
            ),
            Entry::new(concat!($at, ".content[]{input_image}.type"), DISCRIMINATOR),
            Entry::fact_with(
                concat!($at, ".content[]{input_image}.image_url"),
                "image",
                "in",
                "blob",
                Decode::DataUri,
                DATA_URI_TYPE,
            ),
            Entry::fact(
                concat!($at, ".content[]{input_image}.detail"),
                "image",
                "in",
                "detail",
                Decode::Term(DETAIL),
            ),
        ]
    };
}

/// A tool's output she was shown: the exact string, or its parts.
macro_rules! output {
    ($at:expr) => {
        [
            Entry::new(concat!($at, ".type"), DISCRIMINATOR),
            Entry::new(concat!($at, ".call_id"), Class::Correlator),
            Entry::new(concat!($at, ".id"), Class::Drop(DropReason::ProviderId)),
            Entry::new(
                concat!($at, ".internal_chat_message_metadata_passthrough"),
                Class::Drop(PASSTHROUGH),
            ),
            Entry::fact(
                concat!($at, ".output"),
                "text",
                "in",
                "payload",
                Decode::Text,
            ),
            Entry::new(concat!($at, ".output"), Class::Drop(DropReason::Empty)),
            Entry::new(concat!($at, ".output[]{input_text}.type"), DISCRIMINATOR),
            Entry::fact(
                concat!($at, ".output[]{input_text}.text"),
                "text",
                "in",
                "payload",
                Decode::Text,
            ),
            Entry::new(concat!($at, ".output[]{input_image}.type"), DISCRIMINATOR),
            Entry::fact_with(
                concat!($at, ".output[]{input_image}.image_url"),
                "image",
                "in",
                "blob",
                Decode::DataUri,
                DATA_URI_TYPE,
            ),
            Entry::fact(
                concat!($at, ".output[]{input_image}.detail"),
                "image",
                "in",
                "detail",
                Decode::Term(DETAIL),
            ),
        ]
    };
}

/// Every response item kind except her own messages, at `$p`.
macro_rules! items {
    ($p:expr) => {
        [
            // Reasoning: the sealed token is hers; the summary a side model's.
            Entry::new(concat!($p, "{reasoning}.type"), DISCRIMINATOR),
            Entry::new(
                concat!($p, "{reasoning}.id"),
                Class::Drop(DropReason::ProviderId),
            ),
            Entry::new(
                concat!($p, "{reasoning}.internal_chat_message_metadata_passthrough"),
                Class::Drop(PASSTHROUGH),
            ),
            // An empty token holds nothing (X7): its empty drop, and the
            // reasoning is a body gap.
            Entry::fact_with(
                concat!($p, "{reasoning}.encrypted_content"),
                "thinking",
                "out",
                "blob",
                Decode::Base64Url,
                REASONING_SEAL,
            )
            .when(&[Cond::SiblingNot("encrypted_content", "")]),
            Entry::new(
                concat!($p, "{reasoning}.encrypted_content"),
                Class::Drop(DropReason::Empty),
            ),
            Entry::new(
                concat!($p, "{reasoning}.content"),
                Class::Drop(DropReason::Empty),
            ),
            Entry::new(
                concat!($p, "{reasoning}.summary"),
                Class::Drop(DropReason::Empty),
            ),
            Entry::new(
                concat!($p, "{reasoning}.summary[]{summary_text}.type"),
                DISCRIMINATOR,
            ),
            Entry::new(
                concat!($p, "{reasoning}.summary[]{summary_text}.text"),
                SUMMARY,
            ),
            // Calls she wrote.
            Entry::new(concat!($p, "{function_call}.type"), DISCRIMINATOR),
            Entry::new(concat!($p, "{function_call}.call_id"), Class::Correlator),
            Entry::new(
                concat!($p, "{function_call}.id"),
                Class::Drop(DropReason::ProviderId),
            ),
            Entry::new(
                concat!(
                    $p,
                    "{function_call}.internal_chat_message_metadata_passthrough"
                ),
                Class::Drop(PASSTHROUGH),
            ),
            Entry::fact(
                concat!($p, "{function_call}.name"),
                "tool-call",
                "out",
                "tool_name",
                Decode::Text,
            ),
            Entry::fact(
                concat!($p, "{function_call}.namespace"),
                "tool-call",
                "out",
                "tool_namespace",
                Decode::Text,
            ),
            Entry::fact(
                concat!($p, "{function_call}.arguments"),
                "tool-call",
                "out",
                "structure",
                Decode::JsonText,
            ),
            Entry::new(concat!($p, "{custom_tool_call}.type"), DISCRIMINATOR),
            Entry::new(concat!($p, "{custom_tool_call}.call_id"), Class::Correlator),
            Entry::new(
                concat!($p, "{custom_tool_call}.id"),
                Class::Drop(DropReason::ProviderId),
            ),
            Entry::new(
                concat!(
                    $p,
                    "{custom_tool_call}.internal_chat_message_metadata_passthrough"
                ),
                Class::Drop(PASSTHROUGH),
            ),
            Entry::new(
                concat!($p, "{custom_tool_call}.status"),
                Class::Drop(STATUS),
            ),
            Entry::fact(
                concat!($p, "{custom_tool_call}.name"),
                "tool-call",
                "out",
                "tool_name",
                Decode::Text,
            ),
            Entry::fact(
                concat!($p, "{custom_tool_call}.input"),
                "tool-call",
                "out",
                "payload",
                Decode::Text,
            ),
            Entry::new(concat!($p, "{local_shell_call}.type"), DISCRIMINATOR),
            Entry::new(concat!($p, "{local_shell_call}.call_id"), Class::Correlator),
            Entry::new(
                concat!($p, "{local_shell_call}.id"),
                Class::Drop(DropReason::ProviderId),
            ),
            Entry::new(
                concat!($p, "{local_shell_call}.status"),
                Class::Drop(STATUS),
            ),
            Entry::fact_with(
                concat!($p, "{local_shell_call}.action"),
                "tool-call",
                "out",
                "structure",
                Decode::Json,
                LOCAL_SHELL,
            ),
            Entry::new(concat!($p, "{web_search_call}.type"), DISCRIMINATOR),
            Entry::new(
                concat!($p, "{web_search_call}.id"),
                Class::Drop(DropReason::ProviderId),
            ),
            Entry::new(concat!($p, "{web_search_call}.status"), Class::Drop(STATUS)),
            Entry::fact_with(
                concat!($p, "{web_search_call}.action"),
                "tool-call",
                "out",
                "structure",
                Decode::Json,
                WEB_SEARCH,
            ),
            // Git references the client keeps for undo; never sent.
            Entry::new(concat!($p, "{ghost_snapshot}.type"), DISCRIMINATOR),
            Entry::new(
                concat!($p, "{ghost_snapshot}.ghost_commit"),
                Class::Drop(DropReason::Machinery(
                    "a git snapshot the client keeps for undo; never sent",
                )),
            ),
            // Another agent's message to her, and its sealed form.
            Entry::new(concat!($p, "{agent_message}.type"), DISCRIMINATOR),
            Entry::new(
                concat!($p, "{agent_message}.id"),
                Class::Drop(DropReason::ProviderId),
            ),
            Entry::new(
                concat!(
                    $p,
                    "{agent_message}.internal_chat_message_metadata_passthrough"
                ),
                Class::Drop(PASSTHROUGH),
            ),
            Entry::new(
                concat!($p, "{agent_message}.author"),
                Class::Annotation(&[("raw_author", Decode::Text)]),
            ),
            Entry::new(
                concat!($p, "{agent_message}.recipient"),
                Class::Drop(DropReason::Machinery(
                    "the addressee: the agent whose file this is",
                )),
            ),
            Entry::new(
                concat!($p, "{agent_message}.content[]{input_text}.type"),
                DISCRIMINATOR,
            ),
            Entry::fact(
                concat!($p, "{agent_message}.content[]{input_text}.text"),
                "text",
                "in",
                "payload",
                Decode::Text,
            ),
            Entry::new(
                concat!($p, "{agent_message}.content[]{encrypted_content}.type"),
                DISCRIMINATOR,
            ),
            Entry::fact_with(
                concat!(
                    $p,
                    "{agent_message}.content[]{encrypted_content}.encrypted_content"
                ),
                "text",
                "in",
                "blob",
                Decode::Base64Url,
                CONTENT_SEAL,
            ),
        ]
    };
}

/// Her own messages, where the record holds the phase at `$phase`.
macro_rules! said {
    ($p:expr, $phase:literal) => {
        [
            Entry::new(concat!($p, "{message}{assistant}.type"), DISCRIMINATOR),
            Entry::new(concat!($p, "{message}{assistant}.role"), DISCRIMINATOR),
            Entry::new(
                concat!($p, "{message}{assistant}.id"),
                Class::Drop(DropReason::ProviderId),
            ),
            Entry::new(
                concat!(
                    $p,
                    "{message}{assistant}.internal_chat_message_metadata_passthrough"
                ),
                Class::Drop(PASSTHROUGH),
            ),
            Entry::new(
                concat!($p, "{message}{assistant}.phase"),
                Class::Structure(Structure::Vocabulary(PHASES)),
            ),
            Entry::new(
                concat!($p, "{message}{assistant}.content"),
                Class::Drop(DropReason::Empty),
            ),
            Entry::new(
                concat!($p, "{message}{assistant}.content[]{output_text}.type"),
                DISCRIMINATOR,
            ),
            Entry::fact_with(
                concat!($p, "{message}{assistant}.content[]{output_text}.text"),
                "text",
                "out",
                "payload",
                Decode::Text,
                COMMENTARY,
            )
            .when(&[Cond::Is($phase, "commentary")]),
            Entry::fact_with(
                concat!($p, "{message}{assistant}.content[]{output_text}.text"),
                "text",
                "out",
                "payload",
                Decode::Text,
                ANSWER,
            )
            .when(&[Cond::Is($phase, "final_answer")]),
            Entry::fact(
                concat!($p, "{message}{assistant}.content[]{output_text}.text"),
                "text",
                "out",
                "payload",
                Decode::Text,
            )
            .when(&[Cond::Absent($phase)]),
        ]
    };
}

const BARE_USER: &[Entry] = &shown!("{message}{user}", "text");
const BARE_DEVELOPER: &[Entry] = &shown!("{message}{developer}", "developer");
const BARE_ITEMS: &[Entry] = &items!("");
const BARE_SAID: &[Entry] = &said!("", "phase");
const BARE_OUTPUT: &[Entry] = &output!("{function_call_output}");
const BARE_CUSTOM_OUTPUT: &[Entry] = &output!("{custom_tool_call_output}");

const RI_USER: &[Entry] = &shown!("{response_item}.payload{message}{user}", "text");
const RI_DEVELOPER: &[Entry] = &shown!("{response_item}.payload{message}{developer}", "developer");
const RI_ITEMS: &[Entry] = &items!("{response_item}.payload");
const RI_SAID: &[Entry] = &said!("{response_item}.payload", "payload.phase");
const RI_OUTPUT: &[Entry] = &output!("{response_item}.payload{function_call_output}");
const RI_CUSTOM_OUTPUT: &[Entry] = &output!("{response_item}.payload{custom_tool_call_output}");

const RH_USER: &[Entry] = &shown!(
    "{compacted}.payload.replacement_history[]{message}{user}",
    "text"
);
const RH_DEVELOPER: &[Entry] = &shown!(
    "{compacted}.payload.replacement_history[]{message}{developer}",
    "developer"
);
const RH_ITEMS: &[Entry] = &items!("{compacted}.payload.replacement_history[]");

/// A compaction's own items: its re-presented assistant messages (whose
/// phase sits outside the text item, where no condition reaches) and its
/// sealed summaries.
const RH_OTHER: &[Entry] = &[
    Entry::new(
        "{compacted}.payload.replacement_history[]{message}{assistant}.type",
        DISCRIMINATOR,
    ),
    Entry::new(
        "{compacted}.payload.replacement_history[]{message}{assistant}.role",
        DISCRIMINATOR,
    ),
    Entry::new(
        "{compacted}.payload.replacement_history[]{message}{assistant}.id",
        Class::Drop(DropReason::ProviderId),
    ),
    Entry::new(
        "{compacted}.payload.replacement_history[]{message}{assistant}.internal_chat_message_metadata_passthrough",
        Class::Drop(PASSTHROUGH),
    ),
    Entry::new(
        "{compacted}.payload.replacement_history[]{message}{assistant}.phase",
        Class::Open(
            "the register of a re-presented message sits outside its text item; the copy \
             would lose it",
        ),
    ),
    Entry::new(
        "{compacted}.payload.replacement_history[]{message}{assistant}.content[]{output_text}.type",
        DISCRIMINATOR,
    ),
    Entry::fact(
        "{compacted}.payload.replacement_history[]{message}{assistant}.content[]{output_text}.text",
        "text",
        "out",
        "payload",
        Decode::Text,
    ),
    Entry::new(
        "{compacted}.payload.replacement_history[]{compaction}.type",
        DISCRIMINATOR,
    ),
    Entry::new(
        "{compacted}.payload.replacement_history[]{compaction}.id",
        Class::Drop(DropReason::ProviderId),
    ),
    Entry::new(
        "{compacted}.payload.replacement_history[]{compaction}.internal_chat_message_metadata_passthrough",
        Class::Drop(PASSTHROUGH),
    ),
    Entry::fact_with(
        "{compacted}.payload.replacement_history[]{compaction}.encrypted_content",
        "thinking",
        "in",
        "blob",
        Decode::Base64Url,
        COMPACTION_SEAL,
    ),
    Entry::new(
        "{compacted}.payload.replacement_history[]{compaction_summary}.type",
        DISCRIMINATOR,
    ),
    Entry::fact_with(
        "{compacted}.payload.replacement_history[]{compaction_summary}.encrypted_content",
        "thinking",
        "in",
        "blob",
        Decode::Base64Url,
        COMPACTION_SEAL,
    ),
];

/// The G0/G1 header and state lines.
const LEGACY: &[Entry] = &[
    Entry::new(
        ".id",
        Class::Annotation(&[("source_locator", Decode::Text)]),
    ),
    Entry::new(".timestamp", Class::Drop(TIME)),
    Entry::new(
        ".instructions",
        Class::Drop(DropReason::Mirror(Mirror::Within(USER_TEXTS))),
    ),
    Entry::new(".instructions", Class::Drop(DropReason::Empty)),
    Entry::new(".git", Class::Drop(ENVELOPE)),
    Entry::new(
        ".record_type",
        Class::Drop(DropReason::Machinery("a resume state marker")),
    ),
];

const NOT_GHOST: &[Cond] = &[Cond::IsNot("payload.type", "ghost_snapshot")];
const GHOST: &[Cond] = &[Cond::Is("payload.type", "ghost_snapshot")];

const RESPONSE_ITEM: &[Entry] = &[
    Entry::new("{response_item}.type", DISCRIMINATOR),
    Entry::new("{response_item}.ordinal", Class::Drop(ORDINAL)),
    Entry::new(
        "{response_item}.timestamp",
        Class::Annotation(&[("timestamp", Decode::Timestamp)]),
    )
    .when(NOT_GHOST),
    Entry::new("{response_item}.timestamp", Class::Drop(TIME)).when(GHOST),
    Entry::new(
        "{response_item}.metadata.client_authored",
        Class::Drop(DropReason::Machinery("whether the client wrote the item")),
    ),
    Entry::new(
        "{response_item}.metadata.fallback_token_limit_override",
        Class::Drop(USAGE),
    ),
    Entry::new(
        "{response_item}.metadata.mcp_attribution",
        Class::Drop(DropReason::Machinery(
            "which MCP server a tool came from, for the client",
        )),
    ),
    Entry::new(
        "{response_item}.metadata.user_input_order",
        Class::Drop(ORDINAL),
    ),
    Entry::new(
        "{response_item}.metadata.retained_source",
        Class::Open("a retained copy names its original by provider id; the recall is not built"),
    ),
];

/// X9 stays open on the source's own evidence: the field is not the
/// reviewer's context. codex-rs history/src/guardian_history.rs documents
/// `GuardianHistoryCheckpoint` as "Model-invisible Guardian transcript
/// checkpoints" holding the "Original review evidence, separate from the
/// compacted model conversation": the reviewed thread's pre-compaction
/// items (`ContextManager::guardian_history_checkpoint`,
/// core/src/context_manager/history.rs). The reviewer reads it through
/// `collect_guardian_prompt_items_with_parent_turn` (core/src/guardian/
/// prompt.rs), which renders its own prompt from it; its own messages and
/// reasoning are not in this field.
const GUARDIAN_HISTORY: &str = "the reviewed thread's pre-compaction items kept as review \
     evidence (model-invisible): not the reviewer's own context, and how they enter the \
     reviewer's prompt is unverified";

const COMPACTED: &[Entry] = &[
    Entry::new("{compacted}.type", DISCRIMINATOR),
    Entry::new("{compacted}.ordinal", Class::Drop(ORDINAL)),
    Entry::new(
        "{compacted}.timestamp",
        Class::Annotation(&[("timestamp", Decode::Timestamp)]),
    ),
    Entry::fact(
        "{compacted}.payload.message",
        "text",
        "in",
        "payload",
        Decode::Text,
    ),
    Entry::new(
        "{compacted}.payload.replacement_history",
        Class::Drop(DropReason::Empty),
    ),
    Entry::new(
        "{compacted}.payload.first_window_id",
        Class::Drop(DropReason::ProviderId),
    ),
    Entry::new(
        "{compacted}.payload.previous_window_id",
        Class::Drop(DropReason::ProviderId),
    ),
    Entry::new(
        "{compacted}.payload.window_id",
        Class::Drop(DropReason::ProviderId),
    ),
    Entry::new("{compacted}.payload.window_number", Class::Drop(ORDINAL)),
    Entry::new(
        "{compacted}.payload.compaction_response_id",
        Class::Drop(DropReason::ProviderId),
    ),
    Entry::new(
        "{compacted}.payload.latest_token_usage_record",
        Class::Drop(USAGE),
    ),
    Entry::new(
        "{compacted}.payload.replacement_history_metadata",
        Class::Drop(DropReason::Machinery(
            "client attribution of the replacement items",
        )),
    ),
    Entry::new(
        "{compacted}.payload.resume_metadata",
        Class::Drop(DropReason::Machinery("the settings a resume restores")),
    ),
    Entry::new(
        "{compacted}.payload.retained_context",
        Class::Open("retained messages whose rendering into the new window is unverified"),
    ),
    Entry::new(
        "{compacted}.payload.guardian_history",
        Class::Open(GUARDIAN_HISTORY),
    ),
];

const HEADER_ITEM: &[Cond] = &[
    Cond::Scalar("payload.effort"),
    Cond::IsNot("payload.effort", "null"),
];
const NO_HEADER_ITEM: &[Cond] = &[Cond::Absent("payload.effort")];
const NULL_HEADER_ITEM: &[Cond] = &[Cond::Is("payload.effort", "null")];

const TURN_CONTEXT: &[Entry] = &[
    Entry::new("{turn_context}.type", DISCRIMINATOR),
    Entry::new("{turn_context}.ordinal", Class::Drop(ORDINAL)),
    Entry::new(
        "{turn_context}.timestamp",
        Class::Annotation(&[("timestamp", Decode::Timestamp)]),
    )
    .when(HEADER_ITEM),
    Entry::new("{turn_context}.timestamp", Class::Drop(TIME)).when(NO_HEADER_ITEM),
    Entry::new("{turn_context}.timestamp", Class::Drop(TIME)).when(NULL_HEADER_ITEM),
    // The settings header of every request sampled under it (S1).
    Entry::fact(
        "{turn_context}.payload.effort",
        "system",
        "in",
        "effort",
        Decode::Term(EFFORT),
    ),
    Entry::new(
        "{turn_context}.payload.effort",
        Class::Drop(DropReason::Empty),
    ),
    Entry::new(
        "{turn_context}.payload.model",
        Class::Annotation(&[("raw_model", Decode::Text)]),
    ),
    Entry::new(
        "{turn_context}.payload.summary",
        Class::Drop(DropReason::Machinery(
            "which reasoning summaries the client asks for: a side model writes them, she is \
             not shown the setting",
        )),
    ),
    Entry::new(
        "{turn_context}.payload.collaboration_mode.settings.reasoning_effort",
        Class::Drop(DropReason::Mirror(Mirror::Field("payload.effort"))),
    ),
    Entry::new(
        "{turn_context}.payload.collaboration_mode.settings.model",
        Class::Drop(DropReason::Mirror(Mirror::Field("payload.model"))),
    ),
    Entry::new(
        "{turn_context}.payload.collaboration_mode.settings.developer_instructions",
        Class::Drop(DropReason::Mirror(Mirror::Within(INSTRUCTION_TEXTS))),
    ),
    Entry::new(
        "{turn_context}.payload.collaboration_mode.settings.developer_instructions",
        Class::Drop(DropReason::Empty),
    ),
    Entry::new(
        "{turn_context}.payload.collaboration_mode.mode",
        Class::Drop(SETTING),
    ),
    Entry::new(
        "{turn_context}.payload.user_instructions",
        Class::Drop(DropReason::Mirror(Mirror::Within(INSTRUCTION_TEXTS))),
    ),
    Entry::new(
        "{turn_context}.payload.user_instructions",
        Class::Drop(DropReason::Empty),
    ),
    Entry::new("{turn_context}.payload.cwd", Class::Drop(SETTING)),
    Entry::new("{turn_context}.payload.current_date", Class::Drop(SETTING)),
    Entry::new("{turn_context}.payload.timezone", Class::Drop(SETTING)),
    Entry::new(
        "{turn_context}.payload.workspace_roots",
        Class::Drop(SETTING),
    ),
    Entry::new("{turn_context}.payload.personality", Class::Drop(SETTING)),
    Entry::new(
        "{turn_context}.payload.approval_policy",
        Class::Drop(SETTING),
    ),
    Entry::new(
        "{turn_context}.payload.approvals_reviewer",
        Class::Drop(SETTING),
    ),
    Entry::new(
        "{turn_context}.payload.sandbox_policy",
        Class::Drop(SETTING),
    ),
    Entry::new(
        "{turn_context}.payload.permission_profile",
        Class::Drop(SETTING),
    ),
    Entry::new(
        "{turn_context}.payload.file_system_sandbox_policy",
        Class::Drop(SETTING),
    ),
    Entry::new(
        "{turn_context}.payload.network_sandbox_policy",
        Class::Drop(SETTING),
    ),
    Entry::new(
        "{turn_context}.payload.truncation_policy",
        Class::Drop(SETTING),
    ),
    Entry::new(
        "{turn_context}.payload.realtime_active",
        Class::Drop(SETTING),
    ),
    Entry::new(
        "{turn_context}.payload.multi_agent_mode",
        Class::Drop(SETTING),
    ),
    Entry::new(
        "{turn_context}.payload.multi_agent_version",
        Class::Drop(SETTING),
    ),
    Entry::new(
        "{turn_context}.payload.disabled_plugin_ids",
        Class::Drop(SETTING),
    ),
    Entry::new("{turn_context}.payload.comp_hash", Class::Drop(ENVELOPE)),
    Entry::new(
        "{turn_context}.payload.turn_id",
        Class::Drop(DropReason::ProviderId),
    ),
    Entry::new(
        "{turn_context}.payload.root_turn_id",
        Class::Drop(DropReason::ProviderId),
    ),
];

const SESSION_META: &[Entry] = &[
    Entry::new("{session_meta}.type", DISCRIMINATOR),
    Entry::new("{session_meta}.ordinal", Class::Drop(ORDINAL)),
    Entry::new("{session_meta}.timestamp", Class::Drop(TIME)),
    Entry::new("{session_meta}.payload.timestamp", Class::Drop(TIME)),
    Entry::new(
        "{session_meta}.payload.id",
        Class::Annotation(&[("source_locator", Decode::Text)]),
    ),
    // The system section of every request (`instructions`).
    Entry::fact(
        "{session_meta}.payload.base_instructions.text",
        "system",
        "in",
        "payload",
        Decode::Text,
    ),
    Entry::new(
        "{session_meta}.payload.base_instructions.provenance",
        Class::Drop(DropReason::Machinery(
            "where the client took the system prompt from",
        )),
    ),
    Entry::new(
        "{session_meta}.payload.base_instructions",
        Class::Drop(DropReason::Empty),
    ),
    // The project instructions, sent as a user message (kept).
    Entry::new(
        "{session_meta}.payload.instructions",
        Class::Drop(DropReason::Mirror(Mirror::Within(USER_TEXTS))),
    ),
    Entry::new(
        "{session_meta}.payload.instructions",
        Class::Drop(DropReason::Empty),
    ),
    Entry::new(
        "{session_meta}.payload.forked_from_id",
        Class::Drop(DropReason::ProviderId),
    ),
    Entry::new(
        "{session_meta}.payload.subagent_history_start_ordinal",
        Class::Drop(ORDINAL),
    ),
    // The spawning context lives in another unit: the provider's reference
    // to it is kept for the cross-unit join.
    Entry::new(
        "{session_meta}.payload.parent_thread_id",
        Class::Annotation(&[("pointer", Decode::Text)]),
    ),
    Entry::new(
        "{session_meta}.payload.source.subagent.thread_spawn.parent_thread_id",
        Class::Annotation(&[("pointer", Decode::Text)]),
    ),
    Entry::new(
        "{session_meta}.payload.agent_nickname",
        Class::Annotation(&[("raw_author", Decode::Text)]),
    ),
    Entry::new(
        "{session_meta}.payload.agent_path",
        Class::Annotation(&[("raw_author", Decode::Text)]),
    ),
    Entry::new(
        "{session_meta}.payload.agent_role",
        Class::Annotation(&[("raw_author", Decode::Text)]),
    ),
    Entry::new(
        "{session_meta}.payload.agent_role",
        Class::Drop(DropReason::Empty),
    ),
    Entry::new(
        "{session_meta}.payload.agent_nickname",
        Class::Drop(DropReason::Empty),
    ),
    Entry::new(
        "{session_meta}.payload.agent_path",
        Class::Drop(DropReason::Empty),
    ),
    Entry::new(
        "{session_meta}.payload.source.subagent.thread_spawn.agent_nickname",
        Class::Annotation(&[("raw_author", Decode::Text)]),
    ),
    Entry::new(
        "{session_meta}.payload.source.subagent.thread_spawn.agent_nickname",
        Class::Drop(DropReason::Empty),
    ),
    Entry::new(
        "{session_meta}.payload.source.subagent.thread_spawn.agent_path",
        Class::Annotation(&[("raw_author", Decode::Text)]),
    ),
    Entry::new(
        "{session_meta}.payload.source.subagent.thread_spawn.agent_path",
        Class::Drop(DropReason::Empty),
    ),
    Entry::new(
        "{session_meta}.payload.source.subagent.thread_spawn.agent_role",
        Class::Annotation(&[("raw_author", Decode::Text)]),
    ),
    Entry::new(
        "{session_meta}.payload.source.subagent.thread_spawn.agent_role",
        Class::Drop(DropReason::Empty),
    ),
    Entry::new(
        "{session_meta}.payload.source.subagent.thread_spawn.depth",
        Class::Drop(DropReason::Machinery("how deep the spawn tree is")),
    ),
    Entry::new("{session_meta}.payload.source", Class::Drop(ENVELOPE))
        .when(&[Cond::Scalar("payload.source")]),
    Entry::new("{session_meta}.payload.cwd", Class::Drop(ENVELOPE)),
    Entry::new("{session_meta}.payload.originator", Class::Drop(ENVELOPE)),
    Entry::new("{session_meta}.payload.cli_version", Class::Drop(ENVELOPE)),
    Entry::new(
        "{session_meta}.payload.model_provider",
        Class::Drop(ENVELOPE),
    ),
    Entry::new(
        "{session_meta}.payload.thread_source",
        Class::Drop(ENVELOPE),
    ),
    Entry::new("{session_meta}.payload.history_mode", Class::Drop(ENVELOPE)),
    Entry::new(
        "{session_meta}.payload.multi_agent_version",
        Class::Drop(ENVELOPE),
    ),
    Entry::new(
        "{session_meta}.payload.context_window",
        Class::Drop(ENVELOPE),
    ),
    Entry::new(
        "{session_meta}.payload.creator_account_id",
        Class::Drop(ENVELOPE),
    ),
    Entry::new(
        "{session_meta}.payload.creator_user_id",
        Class::Drop(ENVELOPE),
    ),
    Entry::new(
        "{session_meta}.payload.runtime_workspace_roots",
        Class::Drop(ENVELOPE),
    ),
    Entry::new("{session_meta}.payload.git", Class::Drop(ENVELOPE)),
    Entry::new(
        "{session_meta}.payload.session_id",
        Class::Drop(DropReason::ProviderId),
    ),
];

/// A client event that is wholly UI or bookkeeping, by kind.
macro_rules! ui_event {
    ($kind:literal, $why:expr) => {
        Entry::new(
            concat!("{event_msg}.payload{", $kind, "}"),
            Class::Drop($why),
        )
    };
}

const ERRED: &[Cond] = &[Cond::Present("payload.error.message")];
const NOT_ERRED: &[Cond] = &[Cond::Absent("payload.error.message")];

const EVENT_MSG: &[Entry] = &[
    Entry::new("{event_msg}.type", DISCRIMINATOR),
    Entry::new("{event_msg}.ordinal", Class::Drop(ORDINAL)),
    Entry::new("{event_msg}.timestamp", Class::Drop(TIME)),
    // The client's copies of items the unit holds.
    Entry::new("{event_msg}.payload{agent_message}.type", DISCRIMINATOR),
    Entry::new(
        "{event_msg}.payload{agent_message}.message",
        Class::Drop(DropReason::Mirror(Mirror::Unit(ASSISTANT_TEXTS))),
    ),
    // The client's copy of her message's phase (X1): it must equal the
    // message's own, and where the message holds none it is the register's
    // source.
    Entry::new(
        "{event_msg}.payload{agent_message}.phase",
        Class::Supply(PHASE_COPY),
    ),
    Entry::new(
        "{event_msg}.payload{agent_message}.phase",
        Class::Drop(DropReason::Empty),
    ),
    Entry::new(
        "{event_msg}.payload{agent_message}.memory_citation",
        Class::Drop(DropReason::Empty),
    ),
    Entry::new("{event_msg}.payload{agent_reasoning}.type", DISCRIMINATOR),
    Entry::new(
        "{event_msg}.payload{agent_reasoning}.text",
        Class::Drop(DropReason::Mirror(Mirror::Unit(SUMMARY_TEXTS))),
    ),
    Entry::new("{event_msg}.payload{user_message}.type", DISCRIMINATOR),
    Entry::new(
        "{event_msg}.payload{user_message}.message",
        Class::Drop(DropReason::Mirror(Mirror::Within(USER_TEXTS))),
    ),
    Entry::new(
        "{event_msg}.payload{user_message}.images",
        Class::Drop(DropReason::Empty),
    ),
    Entry::new(
        "{event_msg}.payload{user_message}.images[]",
        Class::Drop(DropReason::Mirror(Mirror::Unit(IMAGE_URLS))),
    ),
    Entry::new(
        "{event_msg}.payload{user_message}.local_images",
        Class::Drop(DropReason::Machinery(
            "where the client read an attached image; the item holds the image",
        )),
    ),
    Entry::new(
        "{event_msg}.payload{user_message}.text_elements",
        Class::Drop(DropReason::Machinery("the composer's placeholders")),
    ),
    Entry::new(
        "{event_msg}.payload{user_message}.audio",
        Class::Drop(DropReason::Empty),
    ),
    Entry::new(
        "{event_msg}.payload{user_message}.local_audio",
        Class::Drop(DropReason::Empty),
    ),
    // Why a turn ended.
    Entry::new("{event_msg}.payload{turn_aborted}.type", DISCRIMINATOR),
    // X5: on her last node of the turn when she produced one, and always a
    // claim of the record (which holds no node).
    Entry::new(
        "{event_msg}.payload{turn_aborted}.reason",
        Class::Annotation(&[
            ("ends", Decode::Term(ABORTED)),
            ("turn_ends", Decode::Term(ABORTED)),
        ]),
    ),
    Entry::new(
        "{event_msg}.payload{turn_aborted}.turn_id",
        Class::Drop(DropReason::ProviderId),
    ),
    Entry::new(
        "{event_msg}.payload{turn_aborted}.started_at",
        Class::Drop(TIME),
    ),
    Entry::new(
        "{event_msg}.payload{turn_aborted}.completed_at",
        Class::Drop(TIME),
    ),
    Entry::new(
        "{event_msg}.payload{turn_aborted}.duration_ms",
        Class::Drop(TIME),
    ),
    Entry::new("{event_msg}.payload{task_complete}.type", DISCRIMINATOR).when(NOT_ERRED),
    Entry::new(
        "{event_msg}.payload{task_complete}.type",
        Class::Annotation(&[("ends", Decode::ConstTerm("error"))]),
    )
    .when(ERRED),
    // X6: why the turn failed is `ends: error` on her last node; whether
    // the client rendered the failure to her is unverified, so there is no
    // fact. The model input is built from recorded response items, and an
    // event is none (codex-rs core/src/session/mod.rs,
    // `record_conversation_items` persists response items, events go
    // through `send_event_raw`).
    Entry::new(
        "{event_msg}.payload{task_complete}.error",
        Class::Drop(DropReason::Machinery(
            "the client's report of a failed turn: an event, never a response item of her \
             window",
        )),
    ),
    Entry::new(
        "{event_msg}.payload{task_complete}.last_agent_message",
        Class::Drop(DropReason::Mirror(Mirror::Unit(ASSISTANT_TEXTS))),
    ),
    Entry::new(
        "{event_msg}.payload{task_complete}.last_agent_message",
        Class::Drop(DropReason::Empty),
    ),
    Entry::new(
        "{event_msg}.payload{task_complete}.turn_id",
        Class::Drop(DropReason::ProviderId),
    ),
    Entry::new(
        "{event_msg}.payload{task_complete}.root_turn_id",
        Class::Drop(DropReason::ProviderId),
    ),
    Entry::new(
        "{event_msg}.payload{task_complete}.started_at",
        Class::Drop(TIME),
    ),
    Entry::new(
        "{event_msg}.payload{task_complete}.completed_at",
        Class::Drop(TIME),
    ),
    Entry::new(
        "{event_msg}.payload{task_complete}.duration_ms",
        Class::Drop(TIME),
    ),
    Entry::new(
        "{event_msg}.payload{task_complete}.time_to_first_token_ms",
        Class::Drop(TIME),
    ),
    // A rollback: structure, read by the window rules.
    Entry::new(
        "{event_msg}.payload{thread_rolled_back}.type",
        DISCRIMINATOR,
    ),
    Entry::new(
        "{event_msg}.payload{thread_rolled_back}.num_turns",
        Class::Structure(Structure::Shape("how many turns the rollback cuts")),
    ),
    // Turn markers and settings: bookkeeping, read by the window rules.
    ui_event!(
        "task_started",
        DropReason::Machinery("a turn started: read by the window rules, holds nothing")
    ),
    ui_event!(
        "thread_settings_applied",
        DropReason::Machinery("the client's copy of the settings each turn_context carries")
    ),
    ui_event!("token_count", USAGE),
    ui_event!(
        "context_compacted",
        DropReason::Machinery("the client's notice of the compaction record")
    ),
    ui_event!("exec_command_end", UI),
    ui_event!("patch_apply_end", UI),
    ui_event!("sub_agent_activity", UI),
    ui_event!("collab_agent_interaction_end", UI),
    ui_event!("collab_agent_spawn_end", UI),
    ui_event!("collab_close_end", UI),
    ui_event!("collab_waiting_end", UI),
    // MCP results: the text the client shows; a resource blob is open.
    Entry::new("{event_msg}.payload{mcp_tool_call_end}.type", DISCRIMINATOR),
    Entry::new(
        "{event_msg}.payload{mcp_tool_call_end}.call_id",
        Class::Drop(DropReason::ProviderId),
    ),
    Entry::new(
        "{event_msg}.payload{mcp_tool_call_end}.duration",
        Class::Drop(TIME),
    ),
    Entry::new(
        "{event_msg}.payload{mcp_tool_call_end}.invocation",
        Class::Drop(UI),
    ),
    Entry::new(
        "{event_msg}.payload{mcp_tool_call_end}.result.Ok.isError",
        Class::Drop(UI),
    ),
    Entry::new(
        "{event_msg}.payload{mcp_tool_call_end}.result.Ok._meta",
        Class::Drop(UI),
    ),
    Entry::new(
        "{event_msg}.payload{mcp_tool_call_end}.result.Ok.content",
        Class::Drop(DropReason::Empty),
    ),
    Entry::new(
        "{event_msg}.payload{mcp_tool_call_end}.result.Ok.content[]{text}",
        Class::Drop(UI),
    ),
    Entry::new(
        "{event_msg}.payload{mcp_tool_call_end}.result.Ok.content[]{resource}",
        Class::Open("whether the call's output item carries an MCP resource's bytes is unverified"),
    ),
    Entry::new(
        "{event_msg}.payload{mcp_tool_call_end}.result.Err",
        Class::Drop(UI),
    ),
    Entry::new("{event_msg}.payload{web_search_end}.type", DISCRIMINATOR),
    Entry::new(
        "{event_msg}.payload{web_search_end}.call_id",
        Class::Drop(DropReason::ProviderId),
    ),
    Entry::new(
        "{event_msg}.payload{web_search_end}.action",
        Class::Drop(UI),
    ),
    Entry::new("{event_msg}.payload{web_search_end}.query", Class::Drop(UI)),
    Entry::new(
        "{event_msg}.payload{web_search_end}.results",
        Class::Open("hosted search results: what reached her window is unrecorded"),
    ),
    Entry::new(
        "{event_msg}.payload{web_search_end}.results",
        Class::Drop(DropReason::Empty),
    ),
    // G4 item events.
    Entry::new("{event_msg}.payload{item_completed}.type", DISCRIMINATOR),
    Entry::new(
        "{event_msg}.payload{item_completed}.thread_id",
        Class::Drop(DropReason::ProviderId),
    ),
    Entry::new(
        "{event_msg}.payload{item_completed}.turn_id",
        Class::Drop(DropReason::ProviderId),
    ),
    Entry::new(
        "{event_msg}.payload{item_completed}.started_at_ms",
        Class::Drop(TIME),
    ),
    Entry::new(
        "{event_msg}.payload{item_completed}.completed_at_ms",
        Class::Drop(TIME),
    ),
    Entry::new(
        "{event_msg}.payload{item_completed}.item{AgentMessage}.type",
        DISCRIMINATOR,
    ),
    Entry::new(
        "{event_msg}.payload{item_completed}.item{AgentMessage}.id",
        Class::Drop(DropReason::ProviderId),
    ),
    Entry::new(
        "{event_msg}.payload{item_completed}.item{AgentMessage}.phase",
        Class::Drop(DropReason::Machinery(
            "the client's copy of the item's phase",
        )),
    ),
    Entry::new(
        "{event_msg}.payload{item_completed}.item{AgentMessage}.delivery",
        Class::Drop(UI),
    ),
    Entry::new(
        "{event_msg}.payload{item_completed}.item{AgentMessage}.content",
        Class::Drop(DropReason::Empty),
    ),
    Entry::new(
        "{event_msg}.payload{item_completed}.item{AgentMessage}.content[]{Text}.type",
        DISCRIMINATOR,
    ),
    Entry::new(
        "{event_msg}.payload{item_completed}.item{AgentMessage}.content[]{Text}.text",
        Class::Drop(DropReason::Mirror(Mirror::Unit(ASSISTANT_TEXTS))),
    ),
    Entry::new(
        "{event_msg}.payload{item_completed}.item{AgentMessage}.questions",
        Class::Open("structured questions: whether a call's arguments mirror them is unverified"),
    ),
    Entry::new(
        "{event_msg}.payload{item_completed}.item{AgentMessage}.questions",
        Class::Drop(DropReason::Empty),
    ),
    Entry::new(
        "{event_msg}.payload{item_completed}.item{Reasoning}.type",
        DISCRIMINATOR,
    ),
    Entry::new(
        "{event_msg}.payload{item_completed}.item{Reasoning}.id",
        Class::Drop(DropReason::ProviderId),
    ),
    Entry::new(
        "{event_msg}.payload{item_completed}.item{Reasoning}.summary_text",
        Class::Drop(DropReason::Empty),
    ),
    Entry::new(
        "{event_msg}.payload{item_completed}.item{Reasoning}.summary_text[]",
        Class::Drop(DropReason::Mirror(Mirror::Unit(SUMMARY_TEXTS))),
    ),
    Entry::new(
        "{event_msg}.payload{item_completed}.item{Reasoning}.raw_content",
        Class::Drop(DropReason::Empty),
    ),
    Entry::new(
        "{event_msg}.payload{item_completed}.item{UserMessage}.type",
        DISCRIMINATOR,
    ),
    Entry::new(
        "{event_msg}.payload{item_completed}.item{UserMessage}.id",
        Class::Drop(DropReason::ProviderId),
    ),
    Entry::new(
        "{event_msg}.payload{item_completed}.item{UserMessage}.client_id",
        Class::Drop(DropReason::ProviderId),
    ),
    Entry::new(
        "{event_msg}.payload{item_completed}.item{UserMessage}.content[]{text}.type",
        DISCRIMINATOR,
    ),
    Entry::new(
        "{event_msg}.payload{item_completed}.item{UserMessage}.content[]{text}.text",
        Class::Drop(DropReason::Mirror(Mirror::Within(USER_TEXTS))),
    ),
    Entry::new(
        "{event_msg}.payload{item_completed}.item{UserMessage}.content[]{text}.text_elements",
        Class::Drop(DropReason::Machinery("the composer's placeholders")),
    ),
    ui_event!("item_completed}.item{CommandExecution", UI),
    ui_event!("item_completed}.item{FileChange", UI),
    ui_event!("item_completed}.item{McpToolCall", UI),
    ui_event!("item_completed}.item{ImageView", UI),
    ui_event!("item_completed}.item{CollabAgentToolCall", UI),
    ui_event!("item_completed}.item{SubAgentActivity", UI),
    ui_event!(
        "item_completed}.item{ContextCompaction",
        DropReason::Machinery("the client's notice of the compaction record")
    ),
    Entry::new(
        "{event_msg}.payload{item_completed}.item{WebSearch}",
        Class::Open("hosted search results: what reached her window is unrecorded"),
    ),
    Entry::new(
        "{event_msg}.payload{item_completed}.item{Extension}",
        Class::Open("hosted search results: what reached her window is unrecorded"),
    ),
    Entry::new(
        "{event_msg}.payload{item_completed}.item{HookPrompt}",
        Class::Open("hook-injected text: whether an item mirrors it is unverified"),
    ),
];

/// `world_state` (G3): the client's model of the environment, rendered
/// into developer and user messages. What the census found inside a kept
/// message is a mirror.
///
/// X10, from the openai/codex source (codex-rs, main at 71a4c070,
/// 2026-11-04): the record is the baseline the client diffs the next state
/// against, never model input. `Session::record_context_updates_and_set_reference_context_item`
/// and `Session::record_step_world_state_if_changed` (core/src/session/mod.rs)
/// render each section's change (`render_diff`) into messages, record them
/// with `record_conversation_items` (which persists them as response items,
/// kept here), and only then persist the `WorldState` record ("Persist state
/// only after any model-visible context generated from it"; "A snapshot can
/// change without producing model-visible ... updates"). So a field the
/// census found in no kept message is the snapshot's own machinery:
const WORLD_STATE_BASELINE: DropReason = DropReason::Machinery(
    "world-state baseline the client diffs against; its rendering is the response items \
     recorded before it (codex-rs core/src/session/mod.rs, \
     record_step_world_state_if_changed)",
);
/// Snapshot fingerprints: `collaboration_mode.instructions` and the
/// permissions' `instructions` are a `WorldStateHash` (core/src/context/
/// world_state/collaboration_mode.rs `CollaborationModeSnapshot`,
/// permissions.rs `PermissionsSnapshot`, the SHA-1 in world_state/mod.rs),
/// the usage hint's snapshot is `MultiAgentUsageHintState::fingerprint`
/// (multi_agent_usage_hint.rs): hashes of text she was shown in a message,
/// not the text.
const WORLD_STATE_HASH: DropReason = DropReason::Machinery(
    "a fingerprint of a section the client rendered into a recorded message; the hash is \
     never model input (codex-rs core/src/context/world_state/mod.rs WorldStateHash)",
);
/// An environment's status: `push_environment_values` (core/src/context/
/// world_state/environment.rs) renders it only as `<status>starting</status>`
/// or `<status>failed</status>` inside the environment message, which is
/// recorded as a response item.
const WORLD_STATE_STATUS: DropReason = DropReason::Machinery(
    "an environment's status in the baseline; its rendering, when it has one, is inside the \
     environment message recorded before it (codex-rs core/src/context/world_state/\
     environment.rs push_environment_values)",
);

const WORLD_STATE: &[Entry] = &[
    Entry::new("{world_state}.type", DISCRIMINATOR),
    Entry::new("{world_state}.ordinal", Class::Drop(ORDINAL)),
    Entry::new("{world_state}.timestamp", Class::Drop(TIME)),
    Entry::new(
        "{world_state}.payload.full",
        Class::Drop(DropReason::Machinery(
            "whether the record is a whole state or a change",
        )),
    ),
    Entry::new(
        "{world_state}.payload.state.agents_md.text",
        Class::Drop(DropReason::Mirror(Mirror::Within(INSTRUCTION_TEXTS))),
    ),
    Entry::new(
        "{world_state}.payload.state.agents_md.directory",
        Class::Drop(DropReason::Mirror(Mirror::Within(INSTRUCTION_TEXTS))),
    ),
    Entry::new(
        "{world_state}.payload.state.host_skills.body",
        Class::Drop(DropReason::Mirror(Mirror::Within(INSTRUCTION_TEXTS))),
    ),
    Entry::new(
        "{world_state}.payload.state.environments.current_date",
        Class::Drop(DropReason::Mirror(Mirror::Within(INSTRUCTION_TEXTS))),
    ),
    Entry::new(
        "{world_state}.payload.state.environments.filesystem",
        Class::Drop(DropReason::Mirror(Mirror::Within(INSTRUCTION_TEXTS))),
    ),
    Entry::new(
        "{world_state}.payload.state.environments.timezone",
        Class::Drop(DropReason::Mirror(Mirror::Within(INSTRUCTION_TEXTS))),
    ),
    Entry::new(
        "{world_state}.payload.state.environments.environments.local.cwd",
        Class::Drop(DropReason::Mirror(Mirror::Within(INSTRUCTION_TEXTS))),
    ),
    Entry::new(
        "{world_state}.payload.state.environments.environments.local.shell",
        Class::Drop(DropReason::Mirror(Mirror::Within(INSTRUCTION_TEXTS))),
    ),
    Entry::new(
        "{world_state}.payload.state.environments.environments.local.status",
        Class::Drop(WORLD_STATE_STATUS),
    ),
    Entry::new(
        "{world_state}.payload.state.environments.subagents",
        Class::Drop(WORLD_STATE_BASELINE),
    ),
    Entry::new(
        "{world_state}.payload.state.environments.subagents",
        Class::Drop(DropReason::Empty),
    ),
    Entry::new(
        "{world_state}.payload.state.collaboration_mode",
        Class::Drop(DropReason::Mirror(Mirror::Within(INSTRUCTION_TEXTS))),
    )
    .when(&[Cond::Scalar("payload.state.collaboration_mode")]),
    Entry::new(
        "{world_state}.payload.state.collaboration_mode.instructions",
        Class::Drop(WORLD_STATE_HASH),
    ),
    Entry::new(
        "{world_state}.payload.state.collaboration_mode.mode",
        Class::Drop(SETTING),
    ),
    Entry::new(
        "{world_state}.payload.state.collaboration_mode.model",
        Class::Drop(SETTING),
    ),
    Entry::new(
        "{world_state}.payload.state.personality.personality",
        Class::Drop(DropReason::Mirror(Mirror::Within(INSTRUCTION_TEXTS))),
    ),
    Entry::new(
        "{world_state}.payload.state.personality.model",
        Class::Drop(SETTING),
    ),
    Entry::new(
        "{world_state}.payload.state.permissions",
        Class::Drop(WORLD_STATE_HASH),
    ),
    Entry::new(
        "{world_state}.payload.state.multi_agent_usage_hint",
        Class::Drop(WORLD_STATE_HASH),
    ),
    Entry::new(
        "{world_state}.payload.state.managed_developer_instructions",
        Class::Drop(DropReason::Empty),
    ),
    Entry::new(
        "{world_state}.payload.state.model_catalog",
        Class::Drop(WORLD_STATE_BASELINE),
    ),
    Entry::new(
        "{world_state}.payload.state.persistent_mode",
        Class::Drop(DropReason::Empty),
    ),
    Entry::new(
        "{world_state}.payload.state.agents_md",
        Class::Drop(DropReason::Empty),
    ),
    Entry::new(
        "{world_state}.payload.state.collaboration_mode",
        Class::Drop(DropReason::Empty),
    ),
    Entry::new(
        "{world_state}.payload.state.context_window_guidance",
        Class::Drop(DropReason::Empty),
    ),
    Entry::new("{world_state}.payload.state.model", Class::Drop(SETTING)),
    Entry::new(
        "{world_state}.payload.state.multi_agent_mode",
        Class::Drop(SETTING),
    ),
    Entry::new(
        "{world_state}.payload.state.apps_instructions",
        Class::Drop(SETTING),
    ),
    Entry::new(
        "{world_state}.payload.state.plugins_instructions",
        Class::Drop(SETTING),
    ),
    Entry::new(
        "{world_state}.payload.state.environments_instructions",
        Class::Drop(SETTING),
    ),
    Entry::new(
        "{world_state}.payload.state.git_attribution",
        Class::Drop(SETTING),
    ),
    Entry::new(
        "{world_state}.payload.state.skills.includeInstructions",
        Class::Drop(SETTING),
    ),
    Entry::new(
        "{world_state}.payload.state.host_skills.includeInstructions",
        Class::Drop(SETTING),
    ),
    Entry::new(
        "{world_state}.payload.state.orchestrator_skills",
        Class::Drop(SETTING),
    ),
    Entry::new("{world_state}.payload.state.realtime", Class::Drop(SETTING)),
];

const OTHER_RECORDS: &[Entry] = &[
    Entry::new("{inter_agent_communication_metadata}.type", DISCRIMINATOR),
    Entry::new(
        "{inter_agent_communication_metadata}.ordinal",
        Class::Drop(ORDINAL),
    ),
    Entry::new(
        "{inter_agent_communication_metadata}.timestamp",
        Class::Drop(TIME),
    ),
    Entry::new(
        "{inter_agent_communication_metadata}.payload.trigger_turn",
        Class::Drop(DropReason::Machinery("whether a message starts a turn")),
    ),
    Entry::new("{token_usage_record}.type", DISCRIMINATOR),
    Entry::new("{token_usage_record}.ordinal", Class::Drop(ORDINAL)),
    Entry::new("{token_usage_record}.timestamp", Class::Drop(TIME)),
    Entry::new("{token_usage_record}.payload", Class::Drop(USAGE)),
];

const fn total(parts: &[&[Entry]]) -> usize {
    let mut n = 0;
    let mut part = 0;
    while part < parts.len() {
        n += parts[part].len();
        part += 1;
    }
    n
}

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

const PARTS: &[&[Entry]] = &[
    BARE_USER,
    BARE_DEVELOPER,
    BARE_ITEMS,
    BARE_SAID,
    BARE_OUTPUT,
    BARE_CUSTOM_OUTPUT,
    RI_USER,
    RI_DEVELOPER,
    RI_ITEMS,
    RI_SAID,
    RI_OUTPUT,
    RI_CUSTOM_OUTPUT,
    RH_USER,
    RH_DEVELOPER,
    RH_ITEMS,
    RH_OTHER,
    LEGACY,
    RESPONSE_ITEM,
    COMPACTED,
    TURN_CONTEXT,
    SESSION_META,
    EVENT_MSG,
    WORLD_STATE,
    OTHER_RECORDS,
];
const LEN: usize = total(PARTS);
const ENTRIES: [Entry; LEN] = concat_entries(PARTS);

const TURN_CONTEXT_RECORD: &[Cond] = &[Cond::Is("type", "turn_context")];
const SESSION_META_RECORD: &[Cond] = &[Cond::Is("type", "session_meta")];
const COMPACTED_RECORD: &[Cond] = &[Cond::Is("type", "compacted")];
const TURN_STARTED: &[Cond] = &[
    Cond::Is("type", "event_msg"),
    Cond::Is("payload.type", "task_started"),
];
const ROLLED_BACK: &[Cond] = &[
    Cond::Is("type", "event_msg"),
    Cond::Is("payload.type", "thread_rolled_back"),
];

/// The Codex rollout field table.
pub const TABLE: FieldTable = FieldTable {
    jsonl: true,
    format: "codex",
    namespace: "codex",
    discriminators: &["type", "role"],
    id_keyed: &[],
    record: "",
    order: Order::File(FileWindows {
        header: TURN_CONTEXT_RECORD,
        prefix: SESSION_META_RECORD,
        replace: COMPACTED_RECORD,
        turn: TURN_STARTED,
        rollback: ROLLED_BACK,
        rollback_count: "payload.num_turns",
    }),
    windows: Windows::NONE,
    gaps: Gaps {
        // A request that records no settings header still had one.
        header: Some(SYSTEM_GAP),
        // Before a system prompt is recorded, it is unknown.
        prefix: &[SYSTEM_GAP],
        // Tool definitions are never recorded.
        after_prefix: &[Gap {
            modality: "tool-definition",
            direction: "in",
            kind: "span",
        }],
        orphan: None,
        cleared: None,
        beside: &BESIDE,
    },
    links: &LINKS,
    places: &[
        // Why a turn ended: on her last node of the turn (X5, X6).
        (
            "{event_msg}.payload{turn_aborted}.reason",
            "ends",
            Place::LastOut,
        ),
        (
            "{event_msg}.payload{task_complete}.type",
            "ends",
            Place::LastOut,
        ),
    ],
    entries: &ENTRIES,
};

/// A system body gap: the settings header or the system prompt, unknown.
const SYSTEM_GAP: Gap = Gap {
    modality: "system",
    direction: "in",
    kind: "body",
};

/// A result's head: a fact that holds no value.
const HEAD: Gap = Gap {
    modality: "tool-result",
    direction: "in",
    kind: "",
};

/// What a server-side search read into her window, unrecorded.
const SPAN: Gap = Gap {
    modality: "",
    direction: "",
    kind: "span",
};

const fn head(path: &'static str) -> GapRule {
    GapRule {
        path,
        when: &[],
        at: Beside::Before,
        gap: HEAD,
        held: true,
    }
}

const fn searched(path: &'static str) -> GapRule {
    GapRule {
        path,
        when: &[],
        at: Beside::After,
        gap: SPAN,
        held: false,
    }
}

/// Her reasoning happened; its token is not recorded.
const fn unsealed(path: &'static str) -> GapRule {
    GapRule {
        path,
        when: &[Cond::SiblingAbsent("encrypted_content")],
        at: Beside::Same,
        gap: Gap {
            modality: "thinking",
            direction: "out",
            kind: "body",
        },
        held: true,
    }
}

/// Her reasoning happened; its token is the empty string (X7).
const fn empty_sealed(path: &'static str) -> GapRule {
    GapRule {
        when: &[Cond::Sibling("encrypted_content", "")],
        ..unsealed(path)
    }
}

const BESIDE: [GapRule; 15] = [
    head("{function_call_output}.call_id"),
    head("{custom_tool_call_output}.call_id"),
    head("{response_item}.payload{function_call_output}.call_id"),
    head("{response_item}.payload{custom_tool_call_output}.call_id"),
    searched("{web_search_call}.action"),
    searched("{response_item}.payload{web_search_call}.action"),
    searched("{compacted}.payload.replacement_history[]{web_search_call}.action"),
    unsealed("{reasoning}.type"),
    unsealed("{response_item}.payload{reasoning}.type"),
    unsealed("{compacted}.payload.replacement_history[]{reasoning}.type"),
    empty_sealed("{reasoning}.type"),
    empty_sealed("{response_item}.payload{reasoning}.type"),
    empty_sealed("{compacted}.payload.replacement_history[]{reasoning}.type"),
    // Before replacement histories: what the client kept is unrecorded,
    // then the summary.
    GapRule {
        path: "{compacted}.payload.message",
        when: &[Cond::IsNot("payload.message", "")],
        at: Beside::Before,
        gap: SPAN,
        held: false,
    },
    // The effort is rendered in an unpublished text and place.
    GapRule {
        path: "{turn_context}.payload.effort",
        when: &[
            Cond::IsNot("payload.effort", "null"),
            Cond::IsNot("payload.effort", ""),
        ],
        at: Beside::Same,
        gap: SYSTEM_GAP,
        held: true,
    },
];

/// Every call she can make that a result answers.
const CALLS: &[&str] = &[
    "{function_call}.call_id",
    "{custom_tool_call}.call_id",
    "{local_shell_call}.call_id",
    "{response_item}.payload{function_call}.call_id",
    "{response_item}.payload{custom_tool_call}.call_id",
    "{response_item}.payload{local_shell_call}.call_id",
    "{compacted}.payload.replacement_history[]{function_call}.call_id",
    "{compacted}.payload.replacement_history[]{custom_tool_call}.call_id",
    "{compacted}.payload.replacement_history[]{local_shell_call}.call_id",
];

const fn answers(from: &'static str) -> Link {
    Link {
        attribute: "responds_to",
        from,
        from_modality: "tool-result",
        to: CALLS,
        to_modality: "tool-call",
    }
}

const LINKS: [Link; 4] = [
    answers("{function_call_output}.call_id"),
    answers("{custom_tool_call_output}.call_id"),
    answers("{response_item}.payload{function_call_output}.call_id"),
    answers("{response_item}.payload{custom_tool_call_output}.call_id"),
];
