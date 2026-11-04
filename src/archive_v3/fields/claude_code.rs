//! Field table for Claude Code transcripts (`projects/**/*.jsonl`, the main
//! session files and the subagent files beside them).
//!
//! It names every field path the 2026-11-04 census found in a 300-file
//! sample spread over mtime, and gives each one class. Rows marked
//! [`Class::Open`] are fields whose rendering into her window is not yet
//! built or not yet verified; they fail the unit, as the contract requires,
//! and each says why. Anything else the table does not name fails too.
//!
//! What her window rendered, by record kind:
//!
//! - `user`: prompts, harness text (`isMeta`), tool results (a head, then
//!   each item as a part). A record shown in the transcript only
//!   (`isVisibleInTranscriptOnly`) was never sent and is dropped whole.
//! - `assistant`: her text, sealed thinking (the signature's bytes; Claude 4
//!   and later readable thinking is a summarizer's gloss, facts.md §2), and
//!   tool calls. A `<synthetic>` record is harness text in her slot: `in`,
//!   `form: voiced`.
//! - `attachment`: exactly its `rendered[]` / `renderedInHumanTurn[]` text
//!   when the client recorded it. Before that, the rendering of each kind is
//!   unknown and the record is open.
//! - `api-request-shape`: the system prompt and the tool definitions she was
//!   given. Request frames (`api-request`, `api-request-blob`) are open.
//! - `system`: a compaction boundary (structure) and an away summary (a
//!   gloss); every other subtype is client machinery.
//! - `ai-title`: a gloss on the session's latest node.
//!
//! Window changes the source marks (`input_transformations`,
//! `context_management.applied_edits`) are structure: they shape the window
//! an `out` fact was sampled from, and hold no value of their own.

use super::{
    Class, Compaction, Cond, Decode, DropReason, Entry, FieldTable, Mirror, Order, Requests,
    Structure, Windows,
};
use crate::schemas::blockdag::content_fact::sealed;

const ENVELOPE: DropReason = DropReason::Machinery(
    "client envelope: working directory, branch, client version, session kind",
);
const CLIENT: DropReason = DropReason::Machinery("client state she never saw");
const STOP_REASONS: &[(&str, &str)] = &[
    ("end_turn", "end_turn"),
    ("tool_use", "tool_use"),
    ("stop_sequence", "stop_sequence"),
    ("max_tokens", "max_tokens"),
    ("refusal", "refusal"),
];
const IS_ERROR: &[(&str, &str)] = &[("true", "error"), ("false", "ok")];
const SYNTHETIC: &[Cond] = &[Cond::Is("message.model", "<synthetic>")];
const SAMPLED: &[Cond] = &[Cond::IsNot("message.model", "<synthetic>")];
const VOICED: &[(&str, Decode)] = &[("form", Decode::ConstTerm("voiced"))];
const SIGNATURE: &[(&str, Decode)] = &[(
    "media_type",
    Decode::ConstText(sealed::ANTHROPIC_THINKING_SIGNATURE),
)];
const REDACTED: &[(&str, Decode)] = &[(
    "media_type",
    Decode::ConstText(sealed::ANTHROPIC_REDACTED_THINKING),
)];
const THINKING_SUMMARY: Class = Class::Annotation(&[("gloss:reasoning-summary", Decode::Text)]);

/// Model families whose readable thinking a summarizer wrote (facts.md §2:
/// "the text in a thinking block is never the raw chain of thought" on
/// Claude 4 and later, and "the thinking model does not receive the
/// summarized output"). A model outside this list fails: Claude 3.7's full
/// thinking is unverified from a primary source.
pub const SUMMARIZED_THINKING: &[&str] = &[
    "claude-opus-4",
    "claude-sonnet-4",
    "claude-haiku-4",
    "claude-opus-5",
    "claude-sonnet-5",
    "claude-haiku-5",
    "claude-fable",
];

macro_rules! summarized {
    ($path:expr) => {
        [
            Entry::new($path, THINKING_SUMMARY)
                .when(&[Cond::Prefix("message.model", "claude-opus-4")]),
            Entry::new($path, THINKING_SUMMARY)
                .when(&[Cond::Prefix("message.model", "claude-sonnet-4")]),
            Entry::new($path, THINKING_SUMMARY)
                .when(&[Cond::Prefix("message.model", "claude-haiku-4")]),
            Entry::new($path, THINKING_SUMMARY)
                .when(&[Cond::Prefix("message.model", "claude-opus-5")]),
            Entry::new($path, THINKING_SUMMARY)
                .when(&[Cond::Prefix("message.model", "claude-sonnet-5")]),
            Entry::new($path, THINKING_SUMMARY)
                .when(&[Cond::Prefix("message.model", "claude-haiku-5")]),
            Entry::new($path, THINKING_SUMMARY)
                .when(&[Cond::Prefix("message.model", "claude-fable")]),
        ]
    };
}

/// The envelope of a record that holds nodes: its time and session are
/// annotations of its witnesses.
macro_rules! node_record {
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
            Entry::new(
                concat!("{", $kind, "}.session_id"),
                Class::Drop(DropReason::ProviderId),
            ),
            Entry::new(
                concat!("{", $kind, "}.agentId"),
                Class::Drop(DropReason::ProviderId),
            ),
            Entry::new(concat!("{", $kind, "}.cwd"), Class::Drop(ENVELOPE)),
            Entry::new(concat!("{", $kind, "}.gitBranch"), Class::Drop(ENVELOPE)),
            Entry::new(concat!("{", $kind, "}.version"), Class::Drop(ENVELOPE)),
            Entry::new(concat!("{", $kind, "}.userType"), Class::Drop(ENVELOPE)),
            Entry::new(concat!("{", $kind, "}.entrypoint"), Class::Drop(ENVELOPE)),
            Entry::new(concat!("{", $kind, "}.slug"), Class::Drop(ENVELOPE)),
            Entry::new(concat!("{", $kind, "}.sessionKind"), Class::Drop(ENVELOPE)),
            Entry::new(
                concat!("{", $kind, "}.isSidechain"),
                Class::Drop(DropReason::Machinery(
                    "subagent flag; a subagent file is a unit of its own",
                )),
            ),
        ]
    };
}

/// The items of a content list she was shown, at `$prefix`.
macro_rules! shown_items {
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

const USER: [Entry; 15] = node_record!("user");
const ASSISTANT: [Entry; 15] = node_record!("assistant");
const ATTACHMENT: [Entry; 15] = node_record!("attachment");
const USER_ITEMS: [Entry; 10] = shown_items!("{user}.message.content[]");
const RESULT_ITEMS: [Entry; 10] = shown_items!("{user}.message.content[]{tool_result}.content[]");
const THINKING: [Entry; 7] =
    summarized!("{assistant}.message{message}.content[]{thinking}.thinking");

const USER_FIELDS: &[Entry] = &[
    // Empty readable thinking holds nothing, whatever the model.
    Entry::new(
        "{assistant}.message{message}.content[]{thinking}.thinking",
        Class::Drop(DropReason::Empty),
    ),
    // Shown in the transcript only and never sent: not in any window.
    Entry::new(
        "{user}",
        Class::Drop(DropReason::Machinery(
            "shown in the transcript only; the client never sent it",
        )),
    )
    .when(&[Cond::Is("isVisibleInTranscriptOnly", "true")]),
    Entry::new(
        "{user}.isVisibleInTranscriptOnly",
        Class::Structure(Structure::Vocabulary(&["false"])),
    ),
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
    Entry::new("{user}.message.content", Class::Drop(DropReason::Empty)),
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
    Entry::new(
        "{user}.message.content[]{tool_result}.content",
        Class::Drop(DropReason::Empty),
    ),
    // A tool_reference loads the named tool's definition into her window
    // where it stands.
    Entry::new(
        "{user}.message.content[]{tool_result}.content[]{tool_reference}.type",
        Class::Structure(Structure::Discriminator),
    ),
    Entry::fact(
        "{user}.message.content[]{tool_result}.content[]{tool_reference}.tool_name",
        "tool-definition",
        "in",
        "tool_name",
        Decode::Text,
    ),
    // Harness-written text in the user slot.
    Entry::new(
        "{user}.isMeta",
        Class::Annotation(&[("raw_author", Decode::TextMap(&[("true", "isMeta")]))]),
    )
    .when(&[Cond::Is("isMeta", "true")]),
    Entry::new(
        "{user}.isMeta",
        Class::Structure(Structure::Vocabulary(&["false"])),
    )
    .when(&[Cond::Is("isMeta", "false")]),
    Entry::new(
        "{user}.isCompactSummary",
        Class::Annotation(&[("compaction_summary", Decode::Bool)]),
    )
    .when(&[Cond::Is("isCompactSummary", "true")]),
    Entry::new(
        "{user}.isCompactSummary",
        Class::Structure(Structure::Vocabulary(&["false"])),
    )
    .when(&[Cond::Is("isCompactSummary", "false")]),
    // The message this prompt interrupted ends there.
    Entry::new(
        "{user}.interruptedMessageId",
        Class::Annotation(&[("ends", Decode::ConstTerm("interrupted"))]),
    ),
    Entry::new("{user}.refusedMessageId", Class::Open(NEVER_READ)),
    Entry::new("{user}.sourceToolAssistantUUID", Class::Open(NEVER_READ)),
    Entry::new("{user}.sourceToolUseID", Class::Open(NEVER_READ)),
    Entry::new("{user}.promptId", Class::Drop(DropReason::ProviderId)),
    Entry::new("{user}.imagePasteIds", Class::Drop(DropReason::ProviderId)),
    Entry::new(
        "{user}.scheduledFireId",
        Class::Drop(DropReason::ProviderId),
    ),
    Entry::new(
        "{user}.scheduledTaskId",
        Class::Drop(DropReason::ProviderId),
    ),
    Entry::new(
        "{user}.toolUseResult",
        Class::Drop(DropReason::Machinery(
            "client-side originals and structured copies of a tool result; \
             she saw the tool_result content",
        )),
    ),
    Entry::new("{user}.permissionDecision", Class::Drop(CLIENT)),
    Entry::new("{user}.permissionMode", Class::Drop(CLIENT)),
    Entry::new("{user}.promptSource", Class::Drop(CLIENT)),
    Entry::new("{user}.queuePriority", Class::Drop(CLIENT)),
    Entry::new("{user}.queueSkipAttachments", Class::Drop(CLIENT)),
    Entry::new("{user}.queueSkipSlashCommands", Class::Drop(CLIENT)),
    Entry::new("{user}.sandboxed", Class::Drop(CLIENT)),
    Entry::new("{user}.toolDenialKind", Class::Drop(CLIENT)),
    Entry::new("{user}.toolEndsTurn", Class::Drop(CLIENT)),
    Entry::new("{user}.turnCompanion", Class::Drop(CLIENT)),
    Entry::new("{user}.turnOrigin", Class::Drop(CLIENT)),
    Entry::new("{user}.turnPosition", Class::Drop(CLIENT)),
    Entry::new(
        "{user}.classifierMetaLines",
        Class::Open("classifier lines on a prompt: whether they reached her window is unverified"),
    ),
    Entry::new(
        "{user}.origin.kind",
        Class::Annotation(&[("raw_author", Decode::Text)]),
    ),
    Entry::new(
        "{user}.origin.server",
        Class::Drop(DropReason::Machinery(
            "the channel server that delivered the prompt",
        )),
    ),
    Entry::new(
        "{user}.origin.producer",
        Class::Drop(DropReason::Machinery(
            "the client component that produced the prompt",
        )),
    ),
    Entry::new("{user}.origin.runId", Class::Drop(DropReason::ProviderId)),
    Entry::new(
        "{user}.origin.senderTaskId",
        Class::Drop(DropReason::ProviderId),
    ),
    Entry::new(
        "{user}.origin.from",
        Class::Open("the sender's identity: a relations author, not resolved here"),
    ),
    Entry::new(
        "{user}.origin.name",
        Class::Open("the sender's identity: a relations author, not resolved here"),
    ),
    Entry::new(
        "{user}.origin.body",
        Class::Open("the sender's body: a mirror of the delivered text, not yet checked"),
    ),
];

const ASSISTANT_FIELDS: &[Entry] = &[
    Entry::new("{assistant}.requestId", Class::Drop(DropReason::ProviderId)),
    Entry::new("{assistant}.requestRef", Class::Open(NEVER_READ)),
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
    .when(SAMPLED),
    Entry::new(
        "{assistant}.message{message}.model",
        Class::Annotation(&[("raw_model", Decode::Text)]),
    )
    .when(SYNTHETIC),
    Entry::new(
        "{assistant}.message{message}.stop_reason",
        Class::Annotation(&[("ends", Decode::Term(STOP_REASONS))]),
    )
    .when(&[
        Cond::IsNot("message.stop_reason", "null"),
        Cond::IsNot("message.model", "<synthetic>"),
    ]),
    Entry::new(
        "{assistant}.message{message}.stop_reason",
        Class::Drop(DropReason::Machinery(
            "the stop the harness wrote on text of its own",
        )),
    )
    .when(SYNTHETIC),
    Entry::new(
        "{assistant}.message{message}.stop_reason",
        Class::Drop(DropReason::Empty),
    ),
    Entry::new(
        "{assistant}.message{message}.stop_sequence",
        Class::Drop(DropReason::Empty),
    ),
    Entry::new(
        "{assistant}.message{message}.stop_details",
        Class::Drop(DropReason::Machinery(
            "the refusal classifier's category and explanation; never in her window",
        )),
    ),
    Entry::new(
        "{assistant}.message{message}.usage",
        Class::Drop(DropReason::Machinery("token accounting")),
    ),
    Entry::new(
        "{assistant}.message{message}.diagnostics",
        Class::Drop(DropReason::Machinery("cache diagnostics")),
    ),
    Entry::new("{assistant}.message{message}.container", Class::Drop(DropReason::Empty)),
    // Window changes the API marks: structure of the window she was
    // sampled from.
    Entry::new(
        "{assistant}.message{message}.input_transformations",
        Class::Drop(DropReason::Empty),
    ),
    Entry::new(
        "{assistant}.message{message}.input_transformations[]{thinking_dropped}.type",
        Class::Structure(Structure::Discriminator),
    ),
    Entry::new(
        "{assistant}.message{message}.input_transformations[]{thinking_dropped}.path",
        Class::Structure(Structure::Shape("the dropped thinking block's place in the request")),
    ),
    Entry::new(
        "{assistant}.message{message}.input_transformations[]{thinking_dropped}.reason",
        Class::Structure(Structure::Vocabulary(&[
            "prefix_binding_mismatch",
            "model_binding_mismatch",
            "organization_binding_mismatch",
        ])),
    ),
    Entry::new(
        "{assistant}.message{message}.input_transformations[]{thinking_mismatch_allowed}.type",
        Class::Structure(Structure::Discriminator),
    ),
    Entry::new(
        "{assistant}.message{message}.input_transformations[]{thinking_mismatch_allowed}.path",
        Class::Structure(Structure::Shape("a block that still reached her")),
    ),
    Entry::new(
        "{assistant}.message{message}.input_transformations[]{thinking_mismatch_allowed}.reason",
        Class::Structure(Structure::Vocabulary(&[
            "prefix_binding_mismatch",
            "model_binding_mismatch",
            "organization_binding_mismatch",
        ])),
    ),
    Entry::new(
        "{assistant}.message{message}.context_management",
        Class::Drop(DropReason::Empty),
    ),
    Entry::new(
        "{assistant}.message{message}.context_management.applied_edits",
        Class::Drop(DropReason::Empty),
    ),
    Entry::new(
        "{assistant}.message{message}.context_management.applied_edits[]{clear_tool_uses_20250919}.type",
        Class::Structure(Structure::Discriminator),
    ),
    Entry::new(
        "{assistant}.message{message}.context_management.applied_edits[]{clear_tool_uses_20250919}.cleared_tool_uses",
        Class::Structure(Structure::Shape("how many of the oldest results the server cleared")),
    ),
    Entry::new(
        "{assistant}.message{message}.context_management.applied_edits[]{clear_tool_uses_20250919}.cleared_input_tokens",
        Class::Drop(DropReason::Machinery("token accounting")),
    ),
    Entry::new(
        "{assistant}.message{message}.context_management.applied_edits[]{clear_thinking_20251015}",
        Class::Open("cleared thinking turns: which turns the server cleared is not built"),
    ),
    // Content.
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
    )
    .when(SAMPLED),
    Entry::fact_with(
        "{assistant}.message{message}.content[]{text}.text",
        "text",
        "in",
        "payload",
        Decode::Text,
        VOICED,
    )
    .when(SYNTHETIC),
    Entry::new(
        "{assistant}.message{message}.content[]{thinking}.type",
        Class::Structure(Structure::Discriminator),
    ),
    Entry::fact_with(
        "{assistant}.message{message}.content[]{thinking}.signature",
        "thinking",
        "out",
        "blob",
        Decode::Base64,
        SIGNATURE,
    ),
    Entry::new(
        "{assistant}.message{message}.content[]{redacted_thinking}.type",
        Class::Structure(Structure::Discriminator),
    ),
    Entry::fact_with(
        "{assistant}.message{message}.content[]{redacted_thinking}.data",
        "thinking",
        "out",
        "blob",
        Decode::Base64,
        REDACTED,
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
    // A direct call: its result enters her window as a tool_result. Any
    // other caller fails until its results are placed.
    Entry::new(
        "{assistant}.message{message}.content[]{tool_use}.caller{direct}.type",
        Class::Structure(Structure::Discriminator),
    ),
    Entry::new(
        "{assistant}.message{message}.content[]{fallback}",
        Class::Open("a model switch inside a message: whether it rendered is unverified"),
    ),
    // Synthetic records: the harness's own error, in her slot.
    Entry::new(
        "{assistant}.isApiErrorMessage",
        Class::Annotation(&[("ends", Decode::ConstTerm("error"))]),
    )
    .when(&[Cond::Is("isApiErrorMessage", "true")]),
    Entry::new(
        "{assistant}.isApiErrorMessage",
        Class::Structure(Structure::Vocabulary(&["false"])),
    )
    .when(&[Cond::Is("isApiErrorMessage", "false")]),
    Entry::new("{assistant}.apiErrorStatus", Class::Drop(CLIENT)),
    Entry::new("{assistant}.error", Class::Drop(CLIENT)),
    Entry::new("{assistant}.errorDetails", Class::Drop(CLIENT)),
    Entry::new(
        "{assistant}.wireToolInputs",
        Class::Drop(DropReason::Mirror(Mirror::Keyed {
            array: "message.content",
            key: "id",
            member: "input",
        })),
    ),
    Entry::new("{assistant}.wireIngestContext", Class::Drop(CLIENT)),
    Entry::new("{assistant}.advisorModel", Class::Drop(CLIENT)),
    Entry::new("{assistant}.requestedModel", Class::Drop(CLIENT)),
    Entry::new("{assistant}.apiBlockIndex", Class::Drop(CLIENT)),
    Entry::new("{assistant}.quotaLimits", Class::Drop(CLIENT)),
    Entry::new("{assistant}.serverClassifierRequest", Class::Drop(DropReason::ProviderId)),
    Entry::new("{assistant}.thinkingDisplay", Class::Drop(CLIENT)),
    Entry::new("{assistant}.thinkingDurationMs", Class::Drop(CLIENT)),
    Entry::new("{assistant}.attributionAgent", Class::Drop(CLIENT)),
    Entry::new("{assistant}.attributionMcpServer", Class::Drop(CLIENT)),
    Entry::new("{assistant}.attributionMcpTool", Class::Drop(CLIENT)),
    Entry::new("{assistant}.attributionPlugin", Class::Drop(CLIENT)),
    Entry::new("{assistant}.attributionSkill", Class::Drop(CLIENT)),
    Entry::new(
        "{assistant}.effort",
        Class::Open("effort is rendered into her prompt (facts.md §1); v3 has no home for it yet"),
    ),
    Entry::new(
        "{assistant}.perTurnEffort",
        Class::Open("effort is rendered into her prompt (facts.md §1); v3 has no home for it yet"),
    ),
    Entry::new("{assistant}.perTurnEffort", Class::Drop(DropReason::Empty)),
    Entry::new(
        "{assistant}.truncatedAfterOutput",
        Class::Open("output truncated after it was shown: interrupted or cut is unverified"),
    ),
];

const ATTACHMENT_FIELDS: &[Entry] = &[
    // Client telemetry of a thinking drop the API marked on the message
    // (`input_transformations`); the importer checks the two agree.
    Entry::new(
        "{attachment}",
        Class::Drop(DropReason::Machinery(
            "telemetry of a thinking drop that input_transformations marks",
        )),
    )
    .when(&[Cond::Is("attachment.type", "thinking_drop")]),
    Entry::new(
        "{attachment}.attachment",
        Class::Drop(DropReason::Machinery(
            "the data the rendered text was made from; she saw the rendering",
        )),
    )
    .when(&[Cond::Present("rendered")]),
    Entry::new(
        "{attachment}.attachment",
        Class::Drop(DropReason::Machinery(
            "the data the rendered text was made from; she saw the rendering",
        )),
    )
    .when(&[
        Cond::Absent("rendered"),
        Cond::Present("renderedInHumanTurn"),
    ]),
    Entry::new(
        "{attachment}.attachment",
        Class::Open(
            "an attachment recorded before rendered[]: its rendering per kind is not built",
        ),
    )
    .when(&[
        Cond::Absent("rendered"),
        Cond::Absent("renderedInHumanTurn"),
    ]),
    Entry::fact(
        "{attachment}.rendered[].content",
        "system",
        "in",
        "payload",
        Decode::Text,
    )
    .when(&[Cond::Is("renderedRole", "system")]),
    Entry::fact(
        "{attachment}.rendered[].content",
        "text",
        "in",
        "payload",
        Decode::Text,
    )
    .when(&[Cond::Is("renderedRole", "user")]),
    Entry::new(
        "{attachment}.rendered[].content",
        Class::Open("rendered text with no rendered role: its slot is unverified"),
    )
    .when(&[Cond::Absent("renderedRole")]),
    Entry::fact(
        "{attachment}.renderedInHumanTurn[].content",
        "text",
        "in",
        "payload",
        Decode::Text,
    ),
    Entry::new(
        "{attachment}.renderedRole",
        Class::Annotation(&[("raw_role", Decode::Text)]),
    ),
    Entry::new(
        "{attachment}.renderedBesideToolResult",
        Class::Open(
            "where the attachment renders beside a tool result: the importer never reads it (G3)",
        ),
    ),
];

/// A record kind that is client machinery as a whole.
macro_rules! machinery {
    ($kind:literal, $why:literal) => {
        Entry::new(
            concat!("{", $kind, "}"),
            Class::Drop(DropReason::Machinery($why)),
        )
    };
}

const SYSTEM_FIELDS: &[Entry] = &[
    Entry::new(
        "{system}",
        Class::Drop(DropReason::Machinery("turn timing")),
    )
    .when(&[Cond::Is("subtype", "turn_duration")]),
    Entry::new(
        "{system}",
        Class::Drop(DropReason::Machinery("stop-hook bookkeeping")),
    )
    .when(&[Cond::Is("subtype", "stop_hook_summary")]),
    Entry::new(
        "{system}",
        Class::Drop(DropReason::Machinery("a retried API error")),
    )
    .when(&[Cond::Is("subtype", "api_error")]),
    Entry::new(
        "{system}",
        Class::Drop(DropReason::Machinery(
            "a notice shown to the user, not sent",
        )),
    )
    .when(&[Cond::Is("subtype", "informational")]),
    Entry::new(
        "{system}",
        Class::Drop(DropReason::Machinery(
            "a local command; what she saw of it arrives as a user record",
        )),
    )
    .when(&[Cond::Is("subtype", "local_command")]),
    Entry::new(
        "{system}",
        Class::Drop(DropReason::Machinery("remote-control status")),
    )
    .when(&[Cond::Is("subtype", "bridge_status")]),
    Entry::new(
        "{system}",
        Class::Drop(DropReason::Machinery(
            "a scheduled task fired; its prompt arrives as a user record",
        )),
    )
    .when(&[Cond::Is("subtype", "scheduled_task_fire")]),
    Entry::new(
        "{system}",
        Class::Open(
            "a refusal fallback: its notice, retracted records and model switch are not built",
        ),
    )
    .when(&[Cond::Is("subtype", "model_refusal_fallback")]),
    Entry::new("{system}.type", Class::Structure(Structure::Discriminator)),
    Entry::new(
        "{system}.subtype",
        Class::Structure(Structure::Vocabulary(&["compact_boundary", "away_summary"])),
    ),
    Entry::new("{system}.uuid", Class::Structure(Structure::Order)),
    Entry::new("{system}.parentUuid", Class::Structure(Structure::Order)),
    Entry::new(
        "{system}.timestamp",
        Class::Drop(DropReason::Machinery(
            "a boundary or gloss holds no node of its own",
        )),
    ),
    Entry::new("{system}.sessionId", Class::Drop(DropReason::ProviderId)),
    Entry::new("{system}.session_id", Class::Drop(DropReason::ProviderId)),
    Entry::new("{system}.agentId", Class::Drop(DropReason::ProviderId)),
    Entry::new("{system}.cwd", Class::Drop(ENVELOPE)),
    Entry::new("{system}.gitBranch", Class::Drop(ENVELOPE)),
    Entry::new("{system}.version", Class::Drop(ENVELOPE)),
    Entry::new("{system}.userType", Class::Drop(ENVELOPE)),
    Entry::new("{system}.entrypoint", Class::Drop(ENVELOPE)),
    Entry::new("{system}.slug", Class::Drop(ENVELOPE)),
    Entry::new("{system}.sessionKind", Class::Drop(ENVELOPE)),
    Entry::new("{system}.isSidechain", Class::Drop(ENVELOPE)),
    Entry::new("{system}.isMeta", Class::Drop(CLIENT)),
    Entry::new("{system}.level", Class::Drop(CLIENT)),
    // A compaction: its child is a new root that continues from the old
    // tail, with the preserved segment re-presented after the summary.
    Entry::new(
        "{system}.content",
        Class::Drop(DropReason::Machinery(
            "the client's compaction notice; never sent",
        )),
    )
    .when(&[Cond::Is("subtype", "compact_boundary")]),
    Entry::new("{system}.logicalParentUuid", Class::Correlator),
    Entry::new(
        "{system}.compactMetadata.preservedSegment.headUuid",
        Class::Correlator,
    ),
    Entry::new(
        "{system}.compactMetadata.preservedSegment.tailUuid",
        Class::Correlator,
    ),
    Entry::new(
        "{system}.compactMetadata.preservedSegment.anchorUuid",
        Class::Correlator,
    ),
    Entry::new(
        "{system}.compactMetadata.preservedMessages.uuids[]",
        Class::Correlator,
    ),
    Entry::new(
        "{system}.compactMetadata.preservedMessages.allUuids[]",
        Class::Open(NEVER_READ),
    ),
    Entry::new(
        "{system}.compactMetadata.preservedMessages.anchorUuid",
        Class::Open(NEVER_READ),
    ),
    Entry::new(
        "{system}.compactMetadata.preCompactDiscoveredTools",
        Class::Open(
            "tools loaded before compaction and carried over: their placement is unverified",
        ),
    ),
    Entry::new(
        "{system}.compactMetadata.preCompactDiscoveredTools",
        Class::Drop(DropReason::Empty),
    ),
    Entry::new(
        "{system}.compactMetadata.trigger",
        Class::Drop(DropReason::Machinery("manual or automatic compaction")),
    ),
    Entry::new("{system}.compactMetadata.preTokens", Class::Drop(CLIENT)),
    Entry::new("{system}.compactMetadata.postTokens", Class::Drop(CLIENT)),
    Entry::new("{system}.compactMetadata.durationMs", Class::Drop(CLIENT)),
    Entry::new(
        "{system}.compactMetadata.cumulativeDroppedTokens",
        Class::Drop(CLIENT),
    ),
    Entry::new("{system}.compactMetadata.precomputed", Class::Drop(CLIENT)),
    Entry::new(
        "{system}.compactMetadata.preCompactArtifactReadVersions",
        Class::Drop(CLIENT),
    ),
    // A recap a side model wrote while she was away.
    Entry::new(
        "{system}.content",
        Class::Annotation(&[("gloss:recap", Decode::Text)]),
    )
    .when(&[Cond::Is("subtype", "away_summary")]),
];

const OTHER_RECORDS: &[Entry] = &[
    Entry::new(
        "{ai-title}.type",
        Class::Structure(Structure::Discriminator),
    ),
    Entry::new(
        "{ai-title}.aiTitle",
        Class::Annotation(&[("gloss:title", Decode::Text)]),
    ),
    Entry::new("{ai-title}.sessionId", Class::Drop(DropReason::ProviderId)),
    // The system prompt and tool definitions she was given.
    Entry::new(
        "{api-request-shape}.type",
        Class::Structure(Structure::Discriminator),
    ),
    Entry::new("{api-request-shape}.shapeHash", Class::Open(NEVER_READ)),
    Entry::new(
        "{api-request-shape}.sessionId",
        Class::Drop(DropReason::ProviderId),
    ),
    Entry::new(
        "{api-request-shape}.agentId",
        Class::Drop(DropReason::ProviderId),
    ),
    Entry::new("{api-request-shape}.version", Class::Drop(ENVELOPE)),
    Entry::new(
        "{api-request-shape}.timestamp",
        Class::Annotation(&[("timestamp", Decode::Timestamp)]),
    ),
    Entry::new(
        "{api-request-shape}.shape.system[]{text}.type",
        Class::Structure(Structure::Discriminator),
    ),
    Entry::fact(
        "{api-request-shape}.shape.system[]{text}.text",
        "system",
        "in",
        "payload",
        Decode::Text,
    ),
    Entry::fact(
        "{api-request-shape}.shape.tools[].name",
        "tool-definition",
        "in",
        "tool_name",
        Decode::Text,
    )
    .when(&[Cond::SiblingNot("defer_loading", "true")]),
    Entry::fact(
        "{api-request-shape}.shape.tools[].description",
        "tool-definition",
        "in",
        "payload",
        Decode::Text,
    )
    .when(&[Cond::SiblingNot("defer_loading", "true")]),
    Entry::fact(
        "{api-request-shape}.shape.tools[].input_schema",
        "tool-definition",
        "in",
        "structure",
        Decode::Json,
    )
    .when(&[Cond::SiblingNot("defer_loading", "true")]),
    Entry::new(
        "{api-request-shape}.shape.tools[].name",
        Class::Open(DEFERRED),
    )
    .when(&[Cond::Sibling("defer_loading", "true")]),
    Entry::new(
        "{api-request-shape}.shape.tools[].description",
        Class::Open(DEFERRED),
    )
    .when(&[Cond::Sibling("defer_loading", "true")]),
    Entry::new(
        "{api-request-shape}.shape.tools[].input_schema",
        Class::Open(DEFERRED),
    )
    .when(&[Cond::Sibling("defer_loading", "true")]),
    Entry::new(
        "{api-request-shape}.shape.tools[].defer_loading",
        Class::Structure(Structure::Vocabulary(&["true", "false"])),
    ),
    Entry::new(
        "{api-request-shape}.shape.tools[].eager_input_streaming",
        Class::Drop(DropReason::Machinery("how the client streams call input")),
    ),
    Entry::new(
        "{api-request-shape}.shape.tools[]{advisor_20260301}",
        Class::Open("a server tool: what of it renders into her window is unverified"),
    ),
    Entry::new(
        "{api-request}",
        Class::Open("a request frame: windows from request frames are not built"),
    ),
    Entry::new(
        "{api-request-blob}",
        Class::Open("a request frame message: windows from request frames are not built"),
    ),
    Entry::new(
        "{progress}",
        Class::Open("agent progress may hold the only copy of a subagent; not built"),
    )
    .when(&[Cond::Is("data.type", "agent_progress")]),
    Entry::new(
        "{progress}",
        Class::Drop(DropReason::Machinery("tool progress")),
    )
    .when(&[Cond::IsNot("data.type", "agent_progress")]),
    machinery!(
        "queue-operation",
        "the client's prompt queue; a delivered prompt is a user record"
    ),
    machinery!("last-prompt", "the client's copy of the last prompt"),
    machinery!("mode", "client mode"),
    machinery!("permission-mode", "client permission mode"),
    machinery!("atis-latch", "client state"),
    machinery!("agent-name", "the client's name for the agent"),
    machinery!("agent-setting", "client agent setting"),
    machinery!(
        "custom-title",
        "a title the user set in the client; never in her window"
    ),
    machinery!("frame-link", "a link the client recorded"),
    machinery!("file-history-snapshot", "the client's file backups"),
    machinery!("file-history-delta", "the client's file backups"),
    machinery!("cost-state", "cost accounting"),
    machinery!("tools-used", "the client's tool tally"),
    machinery!(
        "started",
        "a workflow's own log of a phase; her calls and results hold it"
    ),
    machinery!(
        "result",
        "a workflow's own log of a result; her calls and results hold it"
    ),
    machinery!(
        "failed",
        "a workflow's own log of a failure; her calls and results hold it"
    ),
    machinery!("launched", "a workflow's own log of a launch"),
    machinery!(
        "continued-in",
        "a link to the session that continues this one; units are independent (model v3)"
    ),
];

/// A provider id the table once called a correlator although the importer
/// never pairs anything with it. Its meaning in her window is unverified.
const NEVER_READ: &str = "a correlator the importer never reads (G3): its meaning is unverified";

const DEFERRED: &str =
    "a deferred tool: its definition renders where a tool_reference loads it; not built";

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
    + ATTACHMENT.len()
    + USER_ITEMS.len()
    + RESULT_ITEMS.len()
    + THINKING.len()
    + USER_FIELDS.len()
    + ASSISTANT_FIELDS.len()
    + ATTACHMENT_FIELDS.len()
    + SYSTEM_FIELDS.len()
    + OTHER_RECORDS.len();
const ENTRIES: [Entry; LEN] = concat_entries(&[
    &USER,
    &ASSISTANT,
    &ATTACHMENT,
    &USER_ITEMS,
    &RESULT_ITEMS,
    &THINKING,
    USER_FIELDS,
    ASSISTANT_FIELDS,
    ATTACHMENT_FIELDS,
    SYSTEM_FIELDS,
    OTHER_RECORDS,
]);

/// The Claude Code transcript field table.
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
    windows: Windows {
        // The system prompt and tools she was given open every later window.
        prefix: &["api-request-shape"],
        compaction: Some(Compaction {
            shape: "system",
            head: "compactMetadata.preservedSegment.headUuid",
            tail: "compactMetadata.preservedSegment.tailUuid",
            anchor: "compactMetadata.preservedSegment.anchorUuid",
        }),
        // One API message spans the records sharing its id (facts.md 7).
        requests: Some(Requests {
            shape: "assistant",
            id: "message.id",
            dropped: ("message.input_transformations", "thinking_dropped", "path"),
            cleared: (
                "message.context_management.applied_edits",
                "clear_tool_uses_20250919",
                "cleared_tool_uses",
            ),
            result: ("{user}.message.content[]{tool_result}", "tool-result"),
            thinking: "thinking",
        }),
    },
    entries: &ENTRIES,
};
