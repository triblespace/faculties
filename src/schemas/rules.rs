//! Rules schema: the colony's reactive rule environment ("the nervous system").
//!
//! One root collection holds four kinds of facts, as the reactive collection
//! environment requires: rule revisions (authored as text, see
//! `src/rules/*.rules`), one execution reading that selects them, sensor
//! observations, and the driver's concluded requests.
//!
//! A sensor tick is one intrinsically identified entity: its kind, its
//! observation time, and every positive observation made at that moment. The
//! rule language is positive only, so absence is a sensor not publishing, and a
//! rule joins its premises within one tick. An old `quota_ok` can therefore
//! never pair with a newly eligible goal. Conclusions carry no tick in their
//! core, so evaluating the same facts again names the same request.
//!
//! A concluded request is a request, never permission: an effector revalidates
//! under its own lock before acting. The ignite effector (`rules tick
//! --effect`, through `Spawn::ignite`) records each decision as an effect
//! entity beside the request: launched, with the spawn request it wrote, or
//! skipped, with the reason.
//!
//! Every id below was minted with `trible genid` on 2026-11-17. Attributes use
//! the explicit-anchor form, so each value encoding participates in identity;
//! rule text therefore binds attributes through `attribute.id()`, never through
//! these anchor literals.

use triblespace::macros::id_hex;
use triblespace::prelude::*;

/// Stable extrinsic scope for the Rules collection, named `rules`.
///
/// Minted with `trible genid` on 2026-11-17:
/// `61B6EB33E59ED41B719F6FFB925769C1`.
pub const DEFAULT_SCOPE_ID: Id = id_hex!("61B6EB33E59ED41B719F6FFB925769C1");

/// Kind marker of one sensor tick.
///
/// Minted with `trible genid` on 2026-11-17:
/// `0E6BBFAE1DCBA232411D0B5069BA10A9`.
pub const KIND_TICK: Id = id_hex!("0E6BBFAE1DCBA232411D0B5069BA10A9");

/// The environment whose execution reading selects the colony's rules
/// (`reactive::collection::schema::reading_of`).
///
/// Minted with `trible genid` on 2026-11-17:
/// `934A35DA7C68AC968075FF13726ED440`.
pub const ENVIRONMENT_ANCHOR: Id = id_hex!("934A35DA7C68AC968075FF13726ED440");

/// The ignite rule's revision, as written in `src/rules/ignite.rules`.
pub const IGNITE_REVISION: Id = id_hex!("CD7AE21BC2F0E6D765466116C73DF819");

/// The ask-jp rule's revision, as written in `src/rules/ask_jp.rules`.
pub const ASK_JP_REVISION: Id = id_hex!("CDE937E729EEDD5B56BA737AECCC1039");

/// Kind marker of one effector decision about one concluded request.
///
/// Minted with `trible genid` on 2026-11-20:
/// `BB26DE3AF498FE2A477689A58C27C90C`.
pub const KIND_EFFECT: Id = id_hex!("BB26DE3AF498FE2A477689A58C27C90C");

pub mod rules {
    use super::*;
    attributes! {
        /// Sensor: at this tick the goal was eligible for ignition.
        "D2FB20C0266A3F4189A99A5E9BE1484D" as eligible: inlineencodings::GenId;
        /// Sensor: at this tick the weekly Claude quota was under or on pace,
        /// read from this fresh quota observation.
        "A1AD2E962FB16C5ADFB728E7ED962A0E" as quota_ok: inlineencodings::GenId;
        /// Sensor: at this tick this goal was an open `#ask-jp` question.
        "AACF28A0A4A935CF23F0D52DB8E64112" as ask_open: inlineencodings::GenId;
        /// Sensor: at this tick JP had not been asked in the last 24 h; the
        /// value is the last asking (the Unix epoch when never asked).
        "88FB66645F911BB4E88524A86DBEC137" as ask_quiet_since: inlineencodings::NsTAIInterval;
        /// Conclusion core: a request to run a zooid on this goal.
        "9B272F23F6269B7D7429FD0E17DACFCE" as run_for: inlineencodings::GenId;
        /// Conclusion annotation: a tick whose observations supported the request.
        "47576FB2796DFD5220AEE9E8B581D1B5" as requested_in: inlineencodings::GenId;
        /// Conclusion core: a request to nudge JP about the open questions, once
        /// per quiet period since this last asking.
        "47770310366735B6C130B69DCB76C04F" as nudge_after: inlineencodings::NsTAIInterval;
        /// Conclusion annotation: an open question the nudge carries.
        "8A5FAA5D6139AA07FF6274C8AD2D0AA7" as nudge_about: inlineencodings::GenId;
        /// Conclusion annotation: a tick whose observations supported the nudge.
        "4923B4217188AF87C62297E3EEC92EA1" as nudge_in: inlineencodings::GenId;
        /// Effect core: the concluded request this decision is about (minted
        /// 2026-11-20).
        "CBCB8BC516EA38AB31FAA7A947451535" as effect_on: inlineencodings::GenId;
        /// Effect annotation: what the effector did, or why it did nothing.
        "BB912A75C195BC84EC5B80A55369C2AA" as effect_reason: inlineencodings::Handle<blobencodings::UTF8String>;
        /// Effect annotation: the spawn request the effector wrote and kept.
        "236E59B982AE1A38FC157AE60DB2073F" as effect_spawn_request: inlineencodings::GenId;
    }
}
