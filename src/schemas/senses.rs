//! Senses schema: deliberate sensory captures from the Reachy Mini body.
//!
//! Only DELIBERATE captures land here — "I choose to remember this". The
//! continuous perception stream (live camera/mic) stays ephemeral and is
//! never minted into facts (periphery principle): there is no
//! continuous-capture command at all, so the ephemerality is structural,
//! not a policy you could forget to enforce.
//!
//! Each `senses look` / `senses listen` mints one capture entity: the raw
//! payload as a content-addressed blob, plus the modality, mime, geometry,
//! an optional deliberate note ("why I kept this"), and the proprioceptive
//! pose at capture time (JSON head pose / joints) so a future VLA model can
//! ground the frame in the body state that produced it.

use triblespace::macros::id_hex;
use triblespace::prelude::blobencodings::{LongString, RawBytes};
use triblespace::prelude::inlineencodings::{Handle, ShortString, U256BE};
use triblespace::prelude::*;

pub const SENSES_BRANCH_NAME: &str = "senses";

/// Tag for a deliberate sensory capture (one frame or one audio clip).
pub const KIND_CAPTURE: Id = id_hex!("9C26C6EFD09EB2A401EF009FE9229E16");

pub mod capture {
    use super::*;
    attributes! {
        /// The raw payload, content-addressed (PNG frame, WAV clip, …).
        "FC033C3E4E74105D83E8C44004AD8EB7" as pub frame: Handle<RawBytes>;
        /// MIME type of the payload (e.g. "image/png", "audio/wav").
        "ACB762F023B9AF391D914A4F00163192" as pub mime: ShortString;
        /// Pixel width (vision captures).
        "C5251A43428C595C36A276828ECDD232" as pub width: U256BE;
        /// Pixel height (vision captures).
        "D2CE800163450CE0A34AA164AE66E8FF" as pub height: U256BE;
        /// "vision" | "audio" — the sense that produced this capture.
        "11487C7943FB2ED6A675A0E35477A966" as pub modality: ShortString;
        /// Optional deliberate note: why this moment was kept.
        "4E12AEBAB07830F8EEEF997957EA27D4" as pub note: Handle<LongString>;
        /// Proprioceptive pose at capture (JSON: head pose / joints / imu),
        /// so a frame can be grounded in the body state that produced it.
        "509530F784B438714D7A6F2A236F2CFB" as pub pose: Handle<LongString>;
    }
}
