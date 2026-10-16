//! Canonical block-DAG archive schema.
//!
//! Archive data is split into semantic layers plus source-scoped receipts:
//!
//! ```text
//! content fact  -- semantic datum (text, media, tool payload, ...)
//! node          -- one point of experience in the predecessor DAG
//! source projection -- one source's receipt of one node
//! ```
//!
//! The first two layers are structural hash-consing. A node (the entity kind
//! keeps the name `block`) is identified by its predecessors and the content
//! facts it holds, directly: nothing else, no provider id, path, time or
//! position. Sequence is structural only: order is the `previous` chain. A
//! node holds one fact, or an unordered set of facts the source says happened
//! at the same moment. Acts and perceptions are a derived view over the chain
//! ([`crate::blockdag::turns_along`]). Importer receipts are identified by
//! their source namespace and node alone; provider ids, times, paths and
//! actors only annotate them. No source bytes are stored.
//!
//! The block/content-fact ids and attributes preserve the approved
//! `8011a65..a2e575c` design lineage. The scope, media-type, and projection ids
//! below were minted with `trible genid` on 2026-08-08; `holds`, the node's
//! `responds_to` and the receipt's `resolution` on 2026-10-16. The retired
//! content-part layer's ids (`A8A1254C…` contains, `DA0B1A13…` part kind,
//! `63C77505…` ordinal, `28E039E5…` fact, `A7CC0F4A…` responds_to,
//! `884A0F9A…` resolution) are not reused; rows in that older shape are
//! simply not read by this vocabulary.

use triblespace::macros::id_hex;
pub use triblespace::prelude::blobencodings::RawBytes;
use triblespace::prelude::blobencodings::UTF8String;
use triblespace::prelude::inlineencodings::{GenId, Handle, NsTAIInterval, U256BE};
use triblespace::prelude::*;

/// Stable extrinsic scope of the Archive source collection.
pub const DEFAULT_SCOPE_ID: Id = id_hex!("940F76A54E6C2E64469935B9FEF724A1");

/// A content-addressed node of the perception/action DAG.
///
/// A node is one point of experience: one perceived or produced fact, or a
/// set of facts that happened at the same moment. Its identity is its
/// predecessors and its facts; order is the chain of `previous` alone.
pub mod block {
    use super::*;

    attributes! {
        /// IDENTITY. Structural predecessor set. Absent on a root node.
        "9B8F693BE959136E90C34CF054F9033F" unsafe as pub previous: GenId;
        /// NONIDENTITY annotation. A genuine normalized event interval.
        /// Missing or uncertain source time contributes no row; a sentinel is
        /// never substituted. A node may carry several values (a replay
        /// restamps).
        "695A45C4A57FDA7FDF8A609117878E97" unsafe as pub timestamp: NsTAIInterval;
        /// IDENTITY. The content facts the node holds: one, or an unordered
        /// set that happened at the same moment. Minted with `trible genid`
        /// on 2026-10-16.
        "366C896E56C3AC602062A7C241D26FB0" as pub holds: GenId;
        /// NONIDENTITY annotation. The node of the tool call a result node
        /// answers. Minted with `trible genid` on 2026-10-16.
        "E1C88039F93BBA445149820EB55DADED" as pub responds_to: GenId;
    }

    /// Nonidentity kind marker attached after intrinsic node construction.
    pub const KIND: Id = id_hex!("91B88464F7B5A178DC4FA87DE28CDFA9");
}

/// A single perceived or produced semantic datum.
pub mod content_fact {
    use super::*;

    attributes! {
        /// IDENTITY. Reified modality entity.
        "9044EA72B7B056F20CB02375ABFB7D87" unsafe as pub modality: GenId;
        /// IDENTITY. Reified `in` or `out` direction entity: what the agent
        /// perceived and what it produced. This is the training loss mask.
        "3A42C2348E452E2C2E98B6C576576947" unsafe as pub direction: GenId;
        /// IDENTITY for text, thinking, and textual tool payloads.
        "6CA37B269D7900D866824EB5560E747B" unsafe as pub payload: Handle<UTF8String>;
        /// IDENTITY when media bytes are resident.
        "EEA16296F288D635B9BA4F603827E109" unsafe as pub blob: Handle<RawBytes>;
        /// IDENTITY when bytes are not held. External pointers are meaningful
        /// only together with `asset_namespace`.
        "91FB71A7D25EC34E208DA622DD680481" unsafe as pub asset_pointer: Handle<UTF8String>;
        /// IDENTITY with `asset_pointer`. Source namespace of the external
        /// pointer, preventing unrelated vendors from sharing an id string.
        "9B38698BD76DB91C8ED478D304375286" unsafe as pub asset_namespace: GenId;
        /// IDENTITY when known. Link to Files' intrinsic canonical media-type
        /// entity, never an inline MIME spelling. For an unresolved asset it
        /// comes only from the source's own metadata, never from bytes found
        /// beside it.
        "CCEC81B576FF10DDA85C522C6FE86345" unsafe as pub media_type: GenId;
        /// IDENTITY when known. Byte size of an unresolved asset as the source
        /// claims it; recovered bytes never supply it.
        "2CF092F132FA9110BA10B1B1482831DB" unsafe as pub asset_size: U256BE;

        /// NONIDENTITY, monotone. Bytes later recovered for a pointer-identified
        /// fact. Conflicting resolutions are an ambiguity, never last-write-wins.
        "61604A1C2B6AA2A6F1D36541323B0CFE" unsafe as pub resolved_to: Handle<RawBytes>;
    }

    /// Nonidentity kind marker attached after intrinsic fact construction.
    pub const KIND: Id = id_hex!("C29DDE04AD573274192D1AB86BA5B0A3");

    /// Media types of sealed reasoning: bytes only the provider can decrypt,
    /// kept as THINKING blob facts. A `+base64` suffix marks a token whose
    /// base64 spelling did not round-trip, kept as its exact text bytes.
    pub mod sealed {
        pub const OPENAI_REASONING: &str = "application/vnd.openai.encrypted-reasoning";
        pub const OPENAI_COMPACTION: &str = "application/vnd.openai.encrypted-compaction";
        pub const ANTHROPIC_THINKING_SIGNATURE: &str =
            "application/vnd.anthropic.thinking-signature";
        pub const ANTHROPIC_REDACTED_THINKING: &str = "application/vnd.anthropic.redacted-thinking";
    }

    pub mod modality {
        use super::*;
        pub const TEXT: Id = id_hex!("AE8DC7E9948F3B2408F86049F1D2C548");
        pub const AUDIO: Id = id_hex!("BD2EF853BA7CC99A10DDDA2E5C25D1A4");
        pub const IMAGE: Id = id_hex!("2EEF41445AE93BFABD02423BEDB6ECEF");
        pub const TOOL_CALL: Id = id_hex!("A4B7EC585B0906BFFC42464015E2903C");
        pub const TOOL_RESULT: Id = id_hex!("09A663D69EBDEC15ED329EC5CB7AF445");
        pub const THINKING: Id = id_hex!("293748A40FE28BDC888C1AF336F60D5C");
        pub const EVENT: Id = id_hex!("2F13E9308B0760AAFBAC4B1FF959154C");
        /// Generic file payload. Minted with `trible genid` on 2026-08-17.
        pub const FILE: Id = id_hex!("2F241F546612E13BBED8D08048F4298E");
        /// Moving-image payload. Minted with `trible genid` on 2026-08-17.
        pub const VIDEO: Id = id_hex!("F987E4C900B76116CE7E84045DC9F5F4");

        /// Canonical, queryable display vocabulary for Archive modalities.
        pub const SPECS: &[(Id, &str)] = &[
            (TEXT, "text"),
            (AUDIO, "audio"),
            (IMAGE, "image"),
            (FILE, "file"),
            (VIDEO, "video"),
            (TOOL_CALL, "tool-call"),
            (TOOL_RESULT, "tool-result"),
            (THINKING, "thinking"),
            (EVENT, "event"),
        ];
    }

    /// Direction is `in` or `out` only. Everything the agent perceives is
    /// `in` (system prompts, compaction summaries, tool results, unknown
    /// senders, hidden context it was shown); everything it produces is `out`
    /// (text, tool calls, thinking and sealed reasoning).
    pub mod direction {
        use super::*;
        pub const IN: Id = id_hex!("1452B759336E0DEF96E78937D6E7F15D");
        pub const OUT: Id = id_hex!("39990F231B5FDB6F0FF4DB55616A2939");

        /// Canonical, queryable display vocabulary for Archive directions.
        pub const SPECS: &[(Id, &str)] = &[(IN, "in"), (OUT, "out")];
    }
}

/// Source-scoped receipt of one projected node.
///
/// An importer receipt ([`crate::blockdag::block_source_projection`]) has only
/// `source_namespace` and `projects_to` in its identity core, so every
/// occurrence of one message in one source (an original, a fork's replay, a
/// re-export under new provider ids) converges on one receipt. Everything else
/// annotates it. A producer-native receipt
/// ([`crate::blockdag::source_projection`], used by Drive for its own protocol
/// records) also has a locator and a `raw_record` in its core; importers never
/// write `raw_record`, because no source bytes are stored.
pub mod source_projection {
    use super::*;

    attributes! {
        /// IDENTITY. Reified source protocol/namespace.
        "3D9FCD3A68CBFBCFD7375E35D63FB87D" unsafe as pub source_namespace: GenId;
        /// IDENTITY of a producer-native receipt; annotation on an importer
        /// receipt, where it names the conversation (session, rollout or
        /// conversation id) on the leaves of one imported source unit.
        "5F3F80B819EBFB04E2AF20852F9FE3E3" unsafe as pub source_locator: Handle<UTF8String>;
        /// IDENTITY of a producer-native receipt (Drive's own canonical
        /// receipt encoding). Never written by an importer.
        "584415CC85A866E45B91C2673C7F794E" unsafe as pub raw_record: Handle<RawBytes>;
        /// IDENTITY. Canonical block produced by this projection.
        "3810AAD922D0523B9A609E4E1EBA9320" unsafe as pub projects_to: GenId;

        /// NONIDENTITY. Additive receipt evidence supporting the semantic
        /// predecessor classes of the projected block. Written by Drive;
        /// derivable for importer receipts, which therefore omit it.
        "1CE3ACF6B860D6009BA4CB622FBE1DC9" unsafe as pub semantic_predecessor_support: GenId;
        /// NONIDENTITY. Genuine source timestamp claim, when decodable.
        "A6892FD4C7FEB9445C3C5F8E96FC9E6A" unsafe as pub source_timestamp: NsTAIInterval;
        /// NONIDENTITY. Stable Relations entity that produced the occurrence.
        /// This intentionally reuses Archive's already-published generic
        /// author relation id.
        "838CC157FFDD37C6AC7CC5A472E43ADB" unsafe as pub author: GenId;
        /// NONIDENTITY. Stable Relations entity whose stream observed/produced
        /// the occurrence.
        "EBE88D7DDF43CCC594A695504B599AA5" unsafe as pub experiencer: GenId;
        /// NONIDENTITY. Exact vendor author label retained for provenance.
        "01BA2CB9FF56564100D64C81D0391E28" unsafe as pub raw_author: Handle<UTF8String>;
        /// NONIDENTITY. Exact vendor role label retained for provenance.
        "2874AFE6553B4E044D4EBFB8FA89A641" unsafe as pub raw_role: Handle<UTF8String>;
        /// NONIDENTITY. Exact vendor model label retained for provenance.
        "CEB26C9520076D245DF83F77D994A800" unsafe as pub raw_model: Handle<UTF8String>;
        /// NONIDENTITY. Canonical JSON of provider metadata about the
        /// conversation a leaf ends, such as a Claude Code subagent's
        /// `.meta.json`. Minted with `trible genid` on 2026-10-11.
        "8784841BA7FA549AB6898424722F72DA" as pub source_detail: Handle<UTF8String>;
        /// NONIDENTITY. The exact recovered body this occurrence selected for
        /// the external pointer fact its node holds; the handle also occurs
        /// among that fact's `resolved_to` evidence. Minted with `trible
        /// genid` on 2026-10-16.
        "29757DF0058067C0250B586810CA19E0" as pub resolution: Handle<RawBytes>;
        /// NONIDENTITY. The projection rules an importer applied
        /// ([`crate::blockdag::IMPORTER_REVISION`]), so receipts written under
        /// different rules can be told apart. Minted with `trible genid` on
        /// 2026-10-16.
        "F8DA86B1D3E8757161E569AFDC35CF43" as pub importer_revision: Handle<UTF8String>;
    }

    /// Nonidentity kind marker attached after intrinsic receipt construction.
    pub const KIND: Id = id_hex!("F93BD1665DCC2A8EA75054986A2EA148");

    /// Claude Code JSONL source namespace.
    pub const SOURCE_CLAUDE_CODE: Id = id_hex!("08C70B08B8F48CA5C9FAFEA714A4EACE");

    /// Codex app-server rollout JSONL source namespace. Minted with
    /// `trible genid` on 2026-08-16.
    pub const SOURCE_CODEX: Id = id_hex!("C9B3D07DA2B5939383F342B1054E08F3");

    /// ChatGPT data-export source namespace. Minted with `trible genid` on
    /// 2026-08-17.
    pub const SOURCE_CHATGPT: Id = id_hex!("5F2A3161281F3A8EA18589208FE729DA");
    /// Claude Web data-export source namespace. Minted with `trible genid` on
    /// 2026-08-17.
    pub const SOURCE_CLAUDE_WEB: Id = id_hex!("A77D6A2BE2BECFD88641652EBC8EF1D4");
    /// Gemini Takeout activity source namespace. Minted with `trible genid` on
    /// 2026-08-17.
    pub const SOURCE_GEMINI: Id = id_hex!("8F043C104DEF05097364108E8826634F");
    /// GitHub Copilot / VS Code chat-session source namespace. Minted with
    /// `trible genid` on 2026-08-17.
    pub const SOURCE_COPILOT: Id = id_hex!("2D8A49CBF2B2D4D4E705B37C4FFEDB48");
    /// Gemini Antigravity transcript source namespace. Minted with `trible
    /// genid` on 2026-08-17.
    pub const SOURCE_AGY: Id = id_hex!("29D3BFFAF4AC6C2AD2EFD1C7D22B60FB");

    /// Native Drive cognition source namespace. Minted with `trible genid` on
    /// 2026-08-27.
    pub const SOURCE_DRIVE: Id = id_hex!("3F9B847D70B5720F81DD303499F95898");
}
