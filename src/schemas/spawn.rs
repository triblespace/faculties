//! Spawn schema: task zooids started from a brief, and each launch of one.
//!
//! A spawn IS its brief. Its intrinsic identity is the kind tag and the brief
//! handle, so relaunching an identical brief names the same spawn and a
//! changed brief names a new one. Its name (the first 32 bits
//! of the brief handle as two proquints) is an annotation on that entity: a
//! convenience for people and for Relations, never a key anything validates.
//!
//! A run is one launch of a spawn. Its intrinsic identity is the spawn it
//! launched and its start time; everything else a launch knows (the unit,
//! the working directory, the effort, the persona it ran as, who spawned it,
//! the rendered prompt) annotates the run, and settlement later adds the end
//! time, the exit and the report to the same run. Two launches of one brief
//! are two runs, so their exits never mix.
//!
//! Times reuse `metadata::started_at` and `metadata::finished_at`; the
//! spawner reuses the shared author relation `archive::author`.
//!
//! Every id below was minted with `trible genid` on 2026-11-16. Attributes use
//! the explicit-anchor form, so each value encoding participates in identity.

use triblespace::macros::id_hex;
use triblespace::prelude::*;

/// Stable extrinsic scope of the Spawn collection, named `spawn`.
///
/// Minted with `trible genid` on 2026-11-16:
/// `1570DEB13AA10F1631E66D937A731C55`.
pub const DEFAULT_SCOPE_ID: Id = id_hex!("1570DEB13AA10F1631E66D937A731C55");

/// Kind marker of a spawn (one brief).
///
/// Minted with `trible genid` on 2026-11-16:
/// `8F02DF7BB6C50FF3F3BC5024329B4932`.
pub const KIND_SPAWN: Id = id_hex!("8F02DF7BB6C50FF3F3BC5024329B4932");

/// Kind marker of a run (one launch of a spawn).
///
/// Minted with `trible genid` on 2026-11-16:
/// `A44B774609F22742FEE22457D8268CFE`.
pub const KIND_RUN: Id = id_hex!("A44B774609F22742FEE22457D8268CFE");

pub mod spawn {
    use super::*;

    attributes! {
        /// The brief, verbatim: the spawn's identity core and its prompt.
        "49833E5EB9DE7F828EB81CA8072AF599" as brief: inlineencodings::Handle<blobencodings::UTF8String>;
        /// The zooid's name, e.g. `lusab-babad`. Annotation only.
        "B0AF09047B9A884057D1D97DD445A638" as name: inlineencodings::Handle<blobencodings::UTF8String>;
        /// The spawn a run launched.
        "CDC9578C8CA31789C5EA5EBDD7897A35" as of: inlineencodings::GenId;
        /// The Relations persona the run's zooid acted as.
        "2BCEF6826BC5474FCE933A22DB55ACE5" as persona: inlineencodings::GenId;
        /// The systemd user unit the run was started in.
        "11B1B5FFE49B3131764DF6F64A3B5E0A" as unit: inlineencodings::Handle<blobencodings::UTF8String>;
        /// The directory the zooid started in, on the spawning host.
        "94D1C5DA8C9B004162DBB2D8B0FDDF76" as workdir: inlineencodings::Handle<blobencodings::UTF8String>;
        /// The Claude effort level the run was started with.
        "D819F708F4213FA6188A80F778C33C78" as effort: inlineencodings::Handle<blobencodings::UTF8String>;
        /// The whole opening context fed to the zooid: wake, tail, preamble, brief.
        "9E76A94B167864C6C61D1FA7B5369FF2" as prompt: inlineencodings::Handle<blobencodings::UTF8String>;
        /// The main process's exit code, when it exited rather than being killed.
        "CEBA41649EE59E5026D9C77A9D043846" as exit_code: inlineencodings::U256BE;
        /// systemd's verdict on the unit (`success`, `exit-code`, `signal`,
        /// `oom-kill`, `timeout`, ...), exactly as it reported it.
        "05D4D795629D5F72AAC6205C62320DDD" as result: inlineencodings::Handle<blobencodings::UTF8String>;
        /// The signal that ended the main process, when one did (`KILL`, `TERM`, ...).
        "17CFAFF638D79C65700F04D24EE60411" as signal: inlineencodings::Handle<blobencodings::UTF8String>;
        /// The zooid's final reply: claude's stdout.
        "B7536194BBE4B9F40322AE7DB2107528" as report: inlineencodings::Handle<blobencodings::UTF8String>;
    }
}
