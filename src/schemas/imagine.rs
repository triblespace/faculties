//! Imagine schema: what an image generation was asked and answered, and which
//! credential each remote provider uses.
//!
//! Two record kinds:
//!
//! - A generation ([`KIND_GENERATION`]) lives in the **Files** collection, in
//!   the same commit as the file it produced ([`imagine::output`]). It names
//!   the provider, the model, the prompt, every request option as its own
//!   typed fact, and whatever the provider reported back (its creation time,
//!   the size, quality, background and format it chose, token usage, the
//!   request id). Its `metadata::created_at` is when the request was sent.
//!   Readers query the facts they model and ignore the rest; nothing here is
//!   required beyond what a reader asks for.
//! - A credential binding ([`KIND_AUTH`]) lives in the `imagine` collection.
//!   It names one provider and one exact Secrets selector: either a raw
//!   Secrets version, or a structured item version, one exact body of it and
//!   one exact field, together with that field's ciphertext child handle.
//!   Rebinding supersedes the earlier binding through `metadata::supersedes`,
//!   as Discord's authentication revisions do. No plaintext is ever stored.
//!
//! Enumerated request options (quality, output format, background,
//! moderation, FLUX variant, provider) are minted ids rather than strings, so
//! each value is one typed, queryable identity. Every id below was minted with
//! `trible genid` on 2026-11-13.

use crate::secrets::encrypted::Encrypted;
use triblespace::core::blob::encodings::simplearchive::SimpleArchive;
use triblespace::macros::id_hex;
use triblespace::prelude::blobencodings::UTF8String;
use triblespace::prelude::inlineencodings::{Boolean, GenId, Handle, NsTAIInterval, F64, U256BE};
use triblespace::prelude::*;

/// Stable extrinsic scope of the Imagine collection, named `imagine`. It holds
/// credential bindings only; generations live with their files in Files.
pub const DEFAULT_SCOPE_ID: Id = id_hex!("5C4FDE9C1186810B6F898D83F4454726");

/// Kind marker of one image generation (the request and its answer).
pub const KIND_GENERATION: Id = id_hex!("F92C8C4DE252636AC5718BC1C4134F2A");
/// Kind marker of one provider credential binding.
pub const KIND_AUTH: Id = id_hex!("DAFD38B6C02F900E033A111E215E16E6");

/// Provider: OpenAI's Image API (`POST /v1/images/generations`).
pub const PROVIDER_OPENAI: Id = id_hex!("0F6EF63F4D157CCD5D4470C6D48DE753");
/// Provider: the local FLUX.2 pipeline (mary).
pub const PROVIDER_FLUX: Id = id_hex!("689BB40BA551431D4A87621637A74CCD");

/// FLUX.2 variant `klein` (FLUX.2-klein-4B).
pub const VARIANT_KLEIN: Id = id_hex!("DBD550ADE7D87FD7A30B7CCF1F602092");
/// FLUX.2 variant `dev` (FLUX.2-dev).
pub const VARIANT_DEV: Id = id_hex!("CAB78027E4906218908809DFA32A8792");

/// OpenAI `quality`: `low`.
pub const QUALITY_LOW: Id = id_hex!("6B5E1B02B7B8F2B834952E8BA60D7DAE");
/// OpenAI `quality`: `medium`.
pub const QUALITY_MEDIUM: Id = id_hex!("A5E87FDBC61B80B0920BFF3D96EA9B39");
/// OpenAI `quality`: `high`.
pub const QUALITY_HIGH: Id = id_hex!("C417188D5300EFF5A48BE284FC5C17DD");
/// OpenAI `quality`: `xhigh`.
pub const QUALITY_XHIGH: Id = id_hex!("36CDA87C71E82F9B4D32D90CCFD0E3B5");
/// OpenAI `quality`: `max`.
pub const QUALITY_MAX: Id = id_hex!("C1C14516FF339E1F61FF418AB31EF791");
/// OpenAI `quality`: `auto`.
pub const QUALITY_AUTO: Id = id_hex!("EAC076497957C96BAAC64925767E66B0");

/// OpenAI `output_format`: `png`.
pub const FORMAT_PNG: Id = id_hex!("4D526F11F1D0FB954DD4477892471CF8");
/// OpenAI `output_format`: `jpeg`.
pub const FORMAT_JPEG: Id = id_hex!("A774FDDD127B2478AAEF36D093C13372");
/// OpenAI `output_format`: `webp`.
pub const FORMAT_WEBP: Id = id_hex!("F10D8D54DE1E73D1A9E9450780E753E1");

/// OpenAI `background`: `transparent`.
pub const BACKGROUND_TRANSPARENT: Id = id_hex!("E5988CFEF02209C14F01697A5D8CCADF");
/// OpenAI `background`: `opaque`.
pub const BACKGROUND_OPAQUE: Id = id_hex!("71301B7249FB12758E8ED62C597F478B");
/// OpenAI `background`: `auto`.
pub const BACKGROUND_AUTO: Id = id_hex!("93D6FABEE99AD076032367CEE3EB2931");

/// OpenAI `moderation`: `auto`.
pub const MODERATION_AUTO: Id = id_hex!("07AA24D7BDAE88C34DF0C4B244B83029");
/// OpenAI `moderation`: `low`.
pub const MODERATION_LOW: Id = id_hex!("FB19A27149004926445615002DB8C1B0");

pub mod imagine {
    use super::*;

    attributes! {
        /// The Files entity a generation produced.
        "7FB2021F9954AC5302AFB63F35B2A174" as pub output: GenId;
        /// Which provider answered (a generation) or is bound (a credential).
        "A8E674988F7AD519E0D9CD81E8D0B877" as pub provider: GenId;
        /// The model name sent to the provider (or the FLUX checkpoint).
        "B88BEB6E3F066D8B2F5C98CBDE52A4BD" as pub model: Handle<UTF8String>;
        /// The prompt exactly as sent.
        "5877BB5C73230DDF2D7DC63C58EB6738" as pub prompt: Handle<UTF8String>;
        /// A provider's rewritten prompt, when it returns one.
        "31A6127EA2D5692DB071398E3F6AABD3" as pub revised_prompt: Handle<UTF8String>;
        /// The request left the size to the provider (`size: auto`).
        "913FEB369F9360BB9A5B39094D7E08FD" as pub size_auto: Boolean;
        /// Requested width in pixels.
        "6C4BE76E39DF69EF8E11101C8226E65B" as pub width: U256BE;
        /// Requested height in pixels.
        "8296C6C3A7F5BE35BBA5DEC2B2EF2B32" as pub height: U256BE;
        /// Requested quality (one of the `QUALITY_*` ids).
        "3CB1260B020D7D0C63A932A251037096" as pub quality: GenId;
        /// Requested output format (one of the `FORMAT_*` ids).
        "393A4EE1C78612872E046736C35DD8D1" as pub output_format: GenId;
        /// Requested output compression, 0-100 (JPEG and WebP only).
        "77D8F67E8CF076EFD523E2F159FC0D8A" as pub output_compression: U256BE;
        /// Requested background (one of the `BACKGROUND_*` ids).
        "C715B910D651E4A6190368722B77DF98" as pub background: GenId;
        /// Requested moderation level (one of the `MODERATION_*` ids).
        "7F6DBA318C36A743D29E866E2A50D622" as pub moderation: GenId;
        /// The provider's own creation time of the answer.
        "EFA0E509DD6DA7D3C008899878F09AF6" as pub response_created_at: NsTAIInterval;
        /// Width the provider reports it produced.
        "55009DA7781FCF06944DAE132286182E" as pub response_width: U256BE;
        /// Height the provider reports it produced.
        "F937A5751CD4E187D389DC7CCA4C3A92" as pub response_height: U256BE;
        /// Quality the provider reports it used.
        "471104F9F9B84E3301D762DF02A6028C" as pub response_quality: GenId;
        /// Background the provider reports it produced.
        "A912D8D7D976CC15E8D62AE03C33A226" as pub response_background: GenId;
        /// Output format the provider reports it produced.
        "0B7FB1F73358A949D5B6C2F4474EFAF6" as pub response_format: GenId;
        /// The provider's request id (OpenAI's `x-request-id` header).
        "897CA2339C36D501017BB24DDE5D4AF9" as pub request_id: Handle<UTF8String>;
        /// Usage: input tokens.
        "33D5D7BBBE5DC559DEB546691A16D9F1" as pub input_tokens: U256BE;
        /// Usage: output tokens.
        "7C5CBEBD1CC691DC466592869A339668" as pub output_tokens: U256BE;
        /// Usage: total tokens.
        "CC6F149028F75C9A9B35987AF2D36832" as pub total_tokens: U256BE;
        /// Usage: input text tokens.
        "AFDE1609BB8657D64CBE5C268A176EE2" as pub input_text_tokens: U256BE;
        /// Usage: input image tokens.
        "490BBA3DBDB5F0E667EFC717C67659F4" as pub input_image_tokens: U256BE;
        /// Usage: output text tokens.
        "8AF4C9CE0454721B27E07C0DFE2D2BB0" as pub output_text_tokens: U256BE;
        /// Usage: output image tokens.
        "3381F273C6CFA54C0B0AD22BB4688CB2" as pub output_image_tokens: U256BE;
        /// FLUX variant (one of the `VARIANT_*` ids).
        "4C7E7A69054EA4ECBA5401FDCA387924" as pub flux_variant: GenId;
        /// FLUX denoising steps.
        "3A810A6BE245425781B0D8C49B792D20" as pub steps: U256BE;
        /// FLUX seed.
        "477D059574BE62DA06B6D81395E3AB0D" as pub seed: U256BE;
        /// FLUX guidance scale.
        "56CA33BED4EC8FA92E46085E82613802" as pub guidance: F64;
        /// Credential: the exact Secrets version (raw or structured item).
        "2DE6125C6B9C529F6FD5C62D7F1045E3" as pub auth_secret_version: GenId;
        /// Credential: the exact encrypted body of a structured item version.
        "CF258E2754CC5C877A602E40C07F70D3" as pub auth_item_body: Handle<Encrypted<SimpleArchive>>;
        /// Credential: the exact private field of that body.
        "4972940411A78F8A67519C4321DCF13B" as pub auth_item_field: GenId;
        /// Credential: that field's exact ciphertext text child, resolved at
        /// binding time so use opens exactly one child.
        "031534EAC557C6C439FD0FF6FDAAD597" as pub auth_item_child: Handle<Encrypted<UTF8String>>;
    }
}
