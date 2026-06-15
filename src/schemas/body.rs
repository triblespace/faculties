//! Body schema: the Reachy Mini body — perception in, action out — and the
//! deliberate sensory/touch captures it keeps in the pile.
//!
//! Renamed from `senses` (2026-06-16): the faculty is both afferent
//! (perception: look/listen/pose/feel) and efferent (action: gesture/move),
//! the whole embodied loop a vision-language-action model closes. "Body" is
//! the honest name for that loop.
//!
//! Only DELIBERATE captures land here — "I choose to remember this". The
//! continuous perception stream (live camera/mic/encoders) stays ephemeral
//! and is never minted into facts (periphery principle): there is no
//! continuous-capture command, so the ephemerality is structural.
//!
//! Each capture entity carries the raw payload (for vision: a PNG frame; for
//! touch: no payload, the signature lives in `pose`), the modality, optional
//! geometry, an optional deliberate note ("why I kept this"), and the
//! proprioceptive context at capture time (JSON state / touch signature) so a
//! future VLA model can ground the moment in the body state that produced it.

use triblespace::macros::id_hex;
use triblespace::prelude::blobencodings::{LongString, RawBytes};
use triblespace::prelude::inlineencodings::{Handle, ShortString, U256BE};
use triblespace::prelude::*;

pub const BODY_BRANCH_NAME: &str = "body";

/// Tag for a deliberate capture (a frame, an audio clip, or a felt touch).
pub const KIND_CAPTURE: Id = id_hex!("9C26C6EFD09EB2A401EF009FE9229E16");

pub mod capture {
    use super::*;
    attributes! {
        /// The raw payload, content-addressed (PNG frame, WAV clip, …).
        /// Absent on touch captures (the signature lives in `pose`).
        "FC033C3E4E74105D83E8C44004AD8EB7" as pub frame: Handle<RawBytes>;
        /// MIME type of the payload (e.g. "image/png", "audio/wav").
        "ACB762F023B9AF391D914A4F00163192" as pub mime: ShortString;
        /// Pixel width (vision captures).
        "C5251A43428C595C36A276828ECDD232" as pub width: U256BE;
        /// Pixel height (vision captures).
        "D2CE800163450CE0A34AA164AE66E8FF" as pub height: U256BE;
        /// "vision" | "audio" | "touch" — the sense that produced this capture.
        "11487C7943FB2ED6A675A0E35477A966" as pub modality: ShortString;
        /// Optional deliberate note: why this moment was kept.
        "4E12AEBAB07830F8EEEF997957EA27D4" as pub note: Handle<LongString>;
        /// Proprioceptive context at capture (JSON: head pose / joints, or the
        /// touch signature), so a moment can be grounded in the body state
        /// that produced it.
        "509530F784B438714D7A6F2A236F2CFB" as pub pose: Handle<LongString>;
    }
}
