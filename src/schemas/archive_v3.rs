//! Archive model v3: facts, nodes, witnesses and glosses.
//!
//! ```text
//! fact     -- one datum as she experienced it: only what her window rendered
//! node     -- one point of experience in one window (`previous` + `holds`)
//! witness  -- one source held one node (`source_namespace` + `projects_to`)
//! occurrence -- one source record's claims about a witnessed node, kept together
//! gloss    -- text a side model wrote about a node; never hers, never replayed
//! citation -- a span of her text the source marks as citing a uri
//! ```
//!
//! Identity derives from each entity's CORE only. Everything else is an
//! open-world, multi-valued annotation that a reader ignores when it cannot
//! model it. Whether an attribute is core or annotation is part of its
//! documentation below and never depends on how verbose one source is: the
//! importer's per-provider rendering table puts into a fact core exactly what
//! her window rendered, and whatever a source merely claims (an asset size, a
//! pointer's media type, a title she was not shown) goes on the witness.
//!
//! Attributes whose meaning is unchanged from the main schema
//! ([`crate::schemas::blockdag`]) are re-exported here under the same ids, so a
//! v3 text fact, a v3 node and a v3 witness are the same entities main would
//! build for the same content. Every new id below was minted with
//! `trible genid` on 2026-11-04. New attributes use a minted anchor
//! (`"HEX" as name: Encoding`), so their encoding is part of their identity.

use triblespace::macros::id_hex;
use triblespace::prelude::blobencodings::UTF8String;
use triblespace::prelude::inlineencodings::{Boolean, GenId, Handle, I256BE, U256BE};
use triblespace::prelude::*;

/// A fact: one datum as she experienced it.
///
/// CORE, as rendered: `modality`, `direction`, then `payload` (decoded text),
/// `blob` (bytes) and/or `structure` (a `json_tree` root), and where her
/// window rendered them `tool_name`, `tool_namespace`, `status`, `exit_code`,
/// `media_type`, `source_uri`, `title`, `sense`, `detail`, `register`, `form`
/// and, on a gap, `absent`. A fact has no annotations of its own in v3: a
/// claim about it lives on the witness of the node that holds it.
pub mod fact {
    use super::*;

    /// CORE. Resident bytes. Same id as main.
    pub use crate::schemas::blockdag::content_fact::blob;
    /// CORE when rendered. A Files media-type entity with canonical
    /// parameters ([`super::media_type`]). Same id as main; v3 entities carry
    /// their parameters, so a parameter-free type is the Files entity main
    /// already names.
    pub use crate::schemas::blockdag::content_fact::media_type;
    /// CORE. The exact decoded text. Whitespace-only text is kept; `""` is
    /// never stored. Same id as main.
    pub use crate::schemas::blockdag::content_fact::payload;

    /// Nonidentity kind marker attached after construction. Same id as main.
    pub use crate::schemas::blockdag::content_fact::KIND;

    attributes! {
        /// CORE. Reified modality, see [`modality`]. Absent only on a span
        /// gap whose kind is unknown. Main's published id and encoding,
        /// pinned again so the vocabulary module can share its name.
        "9044EA72B7B056F20CB02375ABFB7D87" unsafe as pub modality: GenId;
        /// CORE. `in` or `out`: who produced the fact, never which role slot
        /// it sat in. Absent only on a span gap whose direction is unknown.
        /// Main's published id and encoding, pinned again likewise.
        "3A42C2348E452E2C2E98B6C576576947" unsafe as pub direction: GenId;
        /// CORE. Root of a `json_tree` document (objects order-free) for JSON
        /// a tool defines: call arguments, input schemas, structured results
        /// whose rendering is unknown. Never a JSON string.
        "7DAFC3F9B0F81AE8D09515EA5F91B9E4" as pub structure: GenId;
        /// CORE when rendered. The tool's name on a call, a definition or a
        /// result head.
        "CF83903DDC84A033F62EBADB716FB231" as pub tool_name: Handle<UTF8String>;
        /// CORE when rendered separately from the name: the toolbox (MCP
        /// server, plugin, namespace) the tool belongs to.
        "3CB857C61481889AB0AB9CAF8EC098DC" as pub tool_namespace: Handle<UTF8String>;
        /// CORE when rendered. Result-head status, see [`status`].
        "A9C692ED4D2DA72C1AF09DD1C845803B" as pub status: GenId;
        /// CORE when rendered. A result head's exit code, exactly.
        "46ABC76A496B2F2BAF0C23BC21431B12" as pub exit_code: I256BE;
        /// CORE when rendered, repeatable. A uri she was shown with the item,
        /// or one she cited on an `out` text.
        "658473243A9687FF66A72B7BA264A642" as pub source_uri: Handle<UTF8String>;
        /// CORE when rendered. A name shown with the item: a filename, a
        /// document or page name. A title she was not shown is a witness
        /// claim ([`super::witness::claimed_title`]).
        "75ACF9201C76CF5B3A867C226B27B1F1" as pub title: Handle<UTF8String>;
        /// CORE for sensed input. A [`sense`] entity: the sense that
        /// delivered it, by the label she was shown.
        "11849CD84F69A2EC58C14581F72EE249" as pub sense: GenId;
        /// CORE when rendered. The detail setting an image was rendered
        /// under, see [`detail`].
        "B2575A507E18E6DE11708CB333D3DB03" as pub detail: GenId;
        /// CORE when the channel was rendered. Commentary versus answer on
        /// `out` text, see [`register`].
        "D29B8993800CE0AFAE30C1DBC11AD182" as pub register: GenId;
        /// CORE when it applies. How the fact sat in her stream, see [`form`].
        "0EA4AF23E99142553748A955079C9055" as pub form: GenId;
        /// CORE of a gap fact: something was in a window, its content is
        /// unknown. See [`absent`].
        "D7678E13C0A99A987DFBE7D5AF1C40B3" as pub absent: GenId;
        /// CORE of the system fact that renders a request's settings: the
        /// reasoning effort she was sampled under, see [`effort`]. The
        /// provider renders it into the prompt (facts.md 1); its text and
        /// place are unpublished, so the fact is a `system` body gap that
        /// carries it.
        "38D783B77132356BCE659D32C876D079" as pub effort: GenId;
    }

    /// Reasoning effort, bijective with the source spelling.
    pub mod effort {
        use super::*;
        pub const LOW: Id = id_hex!("B6D71B59F1EE7C41D7A4BC82798BFD82");
        pub const MEDIUM: Id = id_hex!("B219D5C463A640C9F2A4B9AE52616493");
        pub const HIGH: Id = id_hex!("10863E513113BE5B3AE552AD2DFC4E8B");
        pub const XHIGH: Id = id_hex!("DFE2D136739E4DB5B4BAF18F39FE79D8");
        pub const MAX: Id = id_hex!("58D87F5B61B6E9D109FA2E42C457DB8F");

        pub const SPECS: &[(Id, &str)] = &[
            (LOW, "low"),
            (MEDIUM, "medium"),
            (HIGH, "high"),
            (XHIGH, "xhigh"),
            (MAX, "max"),
        ];
    }

    /// Modalities. The first nine keep main's ids; `SYSTEM`, `DEVELOPER`
    /// and `TOOL_DEFINITION` are new.
    pub mod modality {
        use super::*;
        pub use crate::schemas::blockdag::content_fact::modality::{
            AUDIO, EVENT, FILE, IMAGE, TEXT, THINKING, TOOL_CALL, TOOL_RESULT, VIDEO,
        };
        /// A system instruction she was given.
        pub const SYSTEM: Id = id_hex!("018B3CAE8E82FED31FBC058BCE7D7F1C");
        /// A developer instruction she was given.
        pub const DEVELOPER: Id = id_hex!("28216A478EACD25B7DC77FC33F02F149");
        /// A tool definition she read: `tool_name`, optional
        /// `tool_namespace`, the description as `payload`, the input schema
        /// as `structure`.
        pub const TOOL_DEFINITION: Id = id_hex!("61ED3875266868139EF5A58F5ED3EB40");

        pub const SPECS: &[(Id, &str)] = &[
            (TEXT, "text"),
            (THINKING, "thinking"),
            (TOOL_CALL, "tool-call"),
            (TOOL_RESULT, "tool-result"),
            (IMAGE, "image"),
            (AUDIO, "audio"),
            (VIDEO, "video"),
            (FILE, "file"),
            (EVENT, "event"),
            (SYSTEM, "system"),
            (DEVELOPER, "developer"),
            (TOOL_DEFINITION, "tool-definition"),
        ];
    }

    /// `in` (she perceived it) or `out` (she produced it). Main's ids.
    pub mod direction {
        pub use crate::schemas::blockdag::content_fact::direction::{IN, OUT, SPECS};
    }

    /// Result-head status.
    pub mod status {
        use super::*;
        pub const OK: Id = id_hex!("0F59A76FBCFA87AE17133920DC53BDB0");
        pub const ERROR: Id = id_hex!("4AC0108773DAC323CC46CE6D0CDA6AA9");
        /// The tool was still running when the result was shown.
        pub const RUNNING: Id = id_hex!("107017D96E3C8482A0D2C04319905DA3");

        pub const SPECS: &[(Id, &str)] = &[(OK, "ok"), (ERROR, "error"), (RUNNING, "running")];
    }

    /// The detail setting an image was rendered under.
    pub mod detail {
        use super::*;
        pub const LOW: Id = id_hex!("0789A21EDFEEEDE8DA341A0BF00B043C");
        pub const HIGH: Id = id_hex!("B62BA3CC0C973F10713D75B613C55B9C");
        /// The provider chose the resolution; what she perceived is a
        /// function of the image under this setting.
        pub const AUTO: Id = id_hex!("911F7A05F3087CB24E54E1E418759ABE");
        /// The image at its original resolution.
        pub const ORIGINAL: Id = id_hex!("55EE5ABF28DEFF69015B070C20224A27");

        pub const SPECS: &[(Id, &str)] = &[
            (LOW, "low"),
            (HIGH, "high"),
            (AUTO, "auto"),
            (ORIGINAL, "original"),
        ];
    }

    /// The channel of her `out` text, where the source rendered it.
    pub mod register {
        use super::*;
        pub const COMMENTARY: Id = id_hex!("6E0C8B35002104ED509EAA77CC73773B");
        pub const ANSWER: Id = id_hex!("A88BF4A08643E9F8D59D15936C97EEEE");

        pub const SPECS: &[(Id, &str)] = &[(COMMENTARY, "commentary"), (ANSWER, "answer")];
    }

    /// How a fact sat in her stream.
    pub mod form {
        use super::*;
        /// A call she wrote inside her own text: `payload` holds exactly the
        /// bytes she wrote, in emitted order.
        pub const INLINE: Id = id_hex!("110C1546C79718B0C44A70CD999DEF20");
        /// Text the harness put in her slot (a synthetic error, a recall):
        /// always `in`, never trained on as hers.
        pub const VOICED: Id = id_hex!("413099935687BADC91932BD0634EC443");

        pub const SPECS: &[(Id, &str)] = &[(INLINE, "inline"), (VOICED, "voiced")];
    }

    /// Gaps.
    pub mod absent {
        use super::*;
        /// One item whose content is unknown.
        pub const BODY: Id = id_hex!("E7F43FE11C68FBA0D0A32D541C09E49E");
        /// Zero or more items whose content is unknown.
        pub const SPAN: Id = id_hex!("409D49B5F4ECCE4F03DB407544A742D4");

        pub const SPECS: &[(Id, &str)] = &[(BODY, "body"), (SPAN, "span")];
    }
}

/// A sense, as an intrinsic entity `{ metadata::tag: KIND, metadata::name:
/// label }` with the label exactly as she was shown it. The vocabulary is
/// open: a new organ's label is a new entity, not an error.
pub mod sense {
    use super::*;
    /// Identity-core kind tag of a sense entity.
    pub const KIND: Id = id_hex!("59ABC90CF72968C6B50751BCF21B0D8A");
}

/// Media types: Files entities `{ metadata::tag: KIND_MEDIA_TYPE,
/// metadata::name: canonical }`. The canonical spelling lowercases the type,
/// subtype and parameter names, sorts the parameters, and keeps the decoded
/// values exactly (`audio/l16;channels=1;rate=24000`).
pub mod media_type {
    use super::*;
    pub use crate::schemas::files::KIND_MEDIA_TYPE as KIND;

    attributes! {
        /// ANNOTATION. The parameter-free media type of this one (itself
        /// when it has no parameters).
        "FAE8CECE6CA645CDEE4F8B40F66FDAD5" as pub essence: GenId;
    }
}

/// A node: one point of experience in one window.
///
/// CORE: `previous` and `holds`, exactly main's node; one fact per node for
/// anything ordered. Everything else is an ANNOTATION, so that a node's id
/// never depends on another file or on the order of imports.
pub mod node {
    use super::*;

    /// CORE. The fact(s) the node holds.
    pub use crate::schemas::blockdag::block::holds;
    /// CORE. Structural predecessor(s). Absent on a root.
    pub use crate::schemas::blockdag::block::previous;
    /// ANNOTATION. The call node a result node answers.
    pub use crate::schemas::blockdag::block::responds_to;
    /// Nonidentity kind marker attached after construction. Same id as main.
    pub use crate::schemas::blockdag::block::KIND;

    attributes! {
        /// ANNOTATION on a root. The tail of the context this one replaced
        /// (compaction, a Drive epoch, a continued session).
        "DF3238FC838D7872E6170A7FEAEED18A" as pub continues: GenId;
        /// ANNOTATION on a root. The call node that created this context
        /// (a subagent).
        "F7A56A509F4A72CB04997EAFE0A7D0CE" as pub spawned_by: GenId;
        /// ANNOTATION. A re-presented copy points to its original.
        "6D68E246D6D0585336DB523253625D53" as pub recalls: GenId;
        /// ANNOTATION. Why the message (or, for `CUT`, the item) ended here,
        /// see [`ends`]. Whatever was rendered to her about it is an `in`
        /// fact of its own.
        "94D56771E2B6852DEBEB5F413EEA27ED" as pub ends: GenId;
        /// ANNOTATION. How the source recorded what the node holds, see
        /// [`rendering`]. A window holding a node marked `unrecorded` is
        /// kept, and training skips it.
        "53B8C70B6B20F6C148D2D5E8935C603C" as pub rendering: GenId;
    }

    /// How a node's content reached the source.
    pub mod rendering {
        use super::*;
        /// The source kept the data an item was rendered from, not the
        /// rendering she read: the facts hold the data's information as
        /// `in` events, and the exact text she saw is unknown.
        pub const UNRECORDED: Id = id_hex!("481642B1417277F071E58A46D1586111");

        pub const SPECS: &[(Id, &str)] = &[(UNRECORDED, "unrecorded")];
    }

    /// Why a message ended.
    pub mod ends {
        use super::*;

        attributes! {
            /// Vocabulary annotation on an `ends` term: whether the message
            /// is over when an item ends this way.
            "B5357D1DDE5229635B5E7A83638796CC" as pub message_final: Boolean;
        }

        pub const END_TURN: Id = id_hex!("BF542CE99B69B5B1D2B835C33B9E8ACE");
        pub const TOOL_USE: Id = id_hex!("B34558BFAF0F2B714804AC89365D97F0");
        pub const STOP_SEQUENCE: Id = id_hex!("D8CF8E0B0B6E308FC73D14EF207F82AC");
        pub const MAX_TOKENS: Id = id_hex!("C3F36B792DA56C06F33CAA7544AB94E2");
        pub const INTERRUPTED: Id = id_hex!("57C49C73671B0994E2560B7861A2B1FF");
        pub const CLASSIFIER_STOP: Id = id_hex!("E2FBA10A355EF9FDA41E81DECADAE0B5");
        pub const REFUSAL: Id = id_hex!("0E301040E59A029170B9E349CF9FF21D");
        pub const ERROR: Id = id_hex!("541E14547C70E9815DCE855A07880FDF");
        /// Machinery stopped one item short (a thinking budget) and the
        /// message went on.
        pub const CUT: Id = id_hex!("9076195BE23344F81C2A5A23A39C3E57");

        /// `(term, name, message_final)`.
        pub const SPECS: &[(Id, &str, bool)] = &[
            (END_TURN, "end_turn", true),
            (TOOL_USE, "tool_use", true),
            (STOP_SEQUENCE, "stop_sequence", true),
            (MAX_TOKENS, "max_tokens", true),
            (INTERRUPTED, "interrupted", true),
            (CLASSIFIER_STOP, "classifier_stop", true),
            (REFUSAL, "refusal", true),
            (ERROR, "error", true),
            (CUT, "cut", false),
        ];
    }
}

/// A witness: one source held one node.
///
/// CORE: `source_namespace` and `projects_to`, exactly main's importer
/// receipt. What one source record claims about the node (its time, role,
/// model, session) is not annotated on the witness, where the claims of two
/// records holding the same node would mix: each record's claims form one
/// [`super::occurrence`] of the witness.
pub mod witness {
    use super::*;

    /// OCCURRENCE CORE. The model that sampled the node (a vendor model entity).
    pub use crate::schemas::blockdag::source_projection::generated_by;
    /// CORE. The node the source held.
    pub use crate::schemas::blockdag::source_projection::projects_to;
    /// OCCURRENCE CORE. The session, rollout or conversation id.
    pub use crate::schemas::blockdag::source_projection::source_locator;
    /// CORE. The source protocol/namespace.
    pub use crate::schemas::blockdag::source_projection::source_namespace;
    /// OCCURRENCE CORE. A time this source asserted for the node. It lives on
    /// an occurrence so the clock that asserted it (the source) stays known.
    pub use crate::schemas::blockdag::source_projection::source_timestamp as timestamp;
    /// OCCURRENCE CORE tag: the source marks this occurrence a compaction summary.
    pub use crate::schemas::blockdag::source_projection::COMPACTION_SUMMARY;
    /// Nonidentity kind marker attached after construction. Same id as main.
    pub use crate::schemas::blockdag::source_projection::KIND;
    /// OCCURRENCE CORE. Relations entities that produced and observed it.
    pub use crate::schemas::blockdag::source_projection::{author, experiencer};
    /// OCCURRENCE CORE. Exact source spellings.
    pub use crate::schemas::blockdag::source_projection::{raw_author, raw_model, raw_role};

    attributes! {
        /// OCCURRENCE CORE. The provider's reference for content the source does
        /// not hold (an asset pointer, a file id, a template id). Exhaust
        /// only: a gap is resolved by re-importing its unit.
        "837794F3E64E4FEE63470D57780305C2" as pub pointer: Handle<UTF8String>;
        /// OCCURRENCE CORE. A byte size the source claims for the held item.
        "5BB29D9B5ACB1ADB47A068B633137418" as pub claimed_size: U256BE;
        /// OCCURRENCE CORE. A media type the source claims but her window did not
        /// render.
        "04448D64216E7BAAE63543EB84015904" as pub claimed_media_type: GenId;
        /// OCCURRENCE CORE. A title the source gives that her window did not show.
        "7AB9F2216ED53D98D7DF446DE4DF63A1" as pub claimed_title: Handle<UTF8String>;
    }
}

/// An occurrence: the claims one source record makes about a witnessed
/// node, kept together as one tuple.
///
/// CORE: `of` (the witness) and every claim the record makes, with the
/// witness attributes above (`timestamp`, `source_locator`, `raw_role`,
/// `raw_model`, `raw_author`, `generated_by`, `pointer`, the `claimed_*`
/// attributes and the compaction tag). Two records that hold the same node
/// give two occurrences of its one witness unless they claim the same.
pub mod occurrence {
    use super::*;

    /// Nonidentity kind marker attached after construction.
    pub const KIND: Id = id_hex!("D94719250D524DEE209D9FD67C3CEA12");

    attributes! {
        /// CORE. The witness whose node the record held.
        "8AB79AC0A9F1348292D6B75EED5F6D5D" as pub of: GenId;
    }
}

/// A gloss: text a side model (a titler, a recapper, a reasoning summarizer,
/// a reviewer, a memory extractor) wrote about her context. Searchable, never
/// trained on, never replayed.
///
/// CORE: `describes`, `category` and `text`. ANNOTATION: `generated_by`.
pub mod gloss {
    use super::*;

    /// Nonidentity kind marker attached after construction.
    pub const KIND: Id = id_hex!("44C12E666CFBCC2EDEA2C204C0CE4F99");

    /// ANNOTATION. The side model that wrote it.
    pub use crate::schemas::blockdag::source_projection::generated_by;

    attributes! {
        /// CORE. The node the gloss describes: the leaf for a title or
        /// recap, the sealed-thinking node for a reasoning summary, the call
        /// node for a tool-use summary.
        "D656952A816D899177B7EE0585F8F96C" as pub describes: GenId;
        /// CORE. What kind of gloss, see [`category`].
        "C3A82B05A3D80B69E73F620084C0A38A" as pub category: GenId;
        /// CORE. The exact text.
        "C406426B8E56F7A01D4ADE9C280A5833" as pub text: Handle<UTF8String>;
    }

    pub mod category {
        use super::*;
        pub const TITLE: Id = id_hex!("9DEF823D49135FDB4F62E2DE6727411A");
        pub const RECAP: Id = id_hex!("05935744951053B10F161429E4E86F8C");
        pub const REASONING_SUMMARY: Id = id_hex!("DD14F55EFC759B7049F99456BBB07A4F");
        pub const TOOL_USE_SUMMARY: Id = id_hex!("960F9D02A24E3084791B1EEE30264131");
        pub const MEMORY: Id = id_hex!("672FBCCB6A0F157874544CBF6ADEB6D7");
        pub const REVIEW: Id = id_hex!("14672202B6AD24D8048F0D9DBDBE36D4");

        pub const SPECS: &[(Id, &str)] = &[
            (TITLE, "title"),
            (RECAP, "recap"),
            (REASONING_SUMMARY, "reasoning-summary"),
            (TOOL_USE_SUMMARY, "tool-use-summary"),
            (MEMORY, "memory"),
            (REVIEW, "review"),
        ];
    }
}

/// A citation: an annotation entity marking a span of a node's text as
/// citing a uri. Her text is never split at citation offsets.
///
/// Its own id derives from all four fields; it never enters the node's.
/// `start` and `end` are offsets into the payload of the fact the node holds,
/// in the source's own counting.
pub mod citation {
    use super::*;

    /// Nonidentity kind marker attached after construction.
    pub const KIND: Id = id_hex!("D9939953EBB8ED97F86342C36F9D4648");

    attributes! {
        /// The node whose text cites.
        "72F3A60F071BF8A7D90B63C403B06612" as pub cites: GenId;
        /// Span start offset.
        "EF0E0B65B4C6DC272DBBB28EA7FD3247" as pub start: U256BE;
        /// Span end offset.
        "40936D5906DB47BDC503A92CBFB59846" as pub end: U256BE;
        /// The cited uri.
        "6294CB0B8256D1C900401663EA7308DB" as pub uri: Handle<UTF8String>;
    }
}
