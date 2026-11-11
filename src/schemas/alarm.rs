//! Alarm schema: an alarm that wakes JP physically, and the ledger of what
//! was done about it.
//!
//! An alarm is a raise record and the records about it: acks and resolves
//! (either closes it), notes (news attached without restarting the ladder),
//! and steps (one rung of the escalation ladder, written after it ran). Every
//! record is one self-contained intrinsic entity; the records about an alarm
//! name it through [`alarm::of`]. Nothing here is a mutable state: an alarm is
//! open exactly while no ack or resolve names it, so a union of piles can
//! close an alarm and never reopen one.
//!
//! Reused attributes: `metadata::tag` for kinds and classes,
//! `metadata::created_at` for the time of a raise, ack, resolve or note,
//! `metadata::started_at` / `metadata::finished_at` for a step,
//! `metadata::description` for free text (a raise's "what", a resolve's
//! reason, a note, a failed step's error), `metadata::name` on class, rung
//! and via entities, `message::local::from` for who raised, acked, resolved
//! or noted (a Relations person), `quota::host` for the host that raised or
//! ran a step, `discord::message` for the Discord message anchor a step
//! posted or a JP message that acked, and `discord::say_request` for the say
//! request a voice step published.
//!
//! Every id below was minted with `trible genid` on sky on 2026-11-11
//! (compass ac4e2b87). Attributes use the explicit-anchor form, so each value
//! encoding participates in identity.

use triblespace::macros::id_hex;
use triblespace::prelude::*;

/// Stable extrinsic scope for the Alarm collection, named `alarm`.
///
/// Minted with `trible genid` on 2026-11-11:
/// `16F26F0830541F6B8C8C080BB66758A4`.
pub const DEFAULT_SCOPE_ID: Id = id_hex!("16F26F0830541F6B8C8C080BB66758A4");

/// Kind marker on every raise record.
///
/// Minted with `trible genid` on 2026-11-11:
/// `9E1AA740F7555FD47295C0F10E50753E`.
pub const KIND_ALARM: Id = id_hex!("9E1AA740F7555FD47295C0F10E50753E");

/// Kind marker on an acknowledgement: JP (or someone relaying his word) has
/// seen the alarm. Closes it.
///
/// Minted with `trible genid` on 2026-11-11:
/// `4E2A034FB34F5962FBAEBB2516A2434D`.
pub const KIND_ACK: Id = id_hex!("4E2A034FB34F5962FBAEBB2516A2434D");

/// Kind marker on a resolve: someone fixed the cause, so JP no longer needs
/// to be woken. Closes the alarm.
///
/// Minted with `trible genid` on 2026-11-11:
/// `BA6E1060E775C1FB982D2C5C668F7A6C`.
pub const KIND_RESOLVE: Id = id_hex!("BA6E1060E775C1FB982D2C5C668F7A6C");

/// Kind marker on a note: news about an alarm that does not restart its
/// ladder (also what a deduplicated raise becomes).
///
/// Minted with `trible genid` on 2026-11-11:
/// `D3CC6BF340AE35337D532369710D4C0D`.
pub const KIND_NOTE: Id = id_hex!("D3CC6BF340AE35337D532369710D4C0D");

/// Kind marker on a step: one rung of the ladder, recorded after it ran.
///
/// Minted with `trible genid` on 2026-11-11:
/// `83B784CC26C756591772C4BDE5C59F28`.
pub const KIND_STEP: Id = id_hex!("83B784CC26C756591772C4BDE5C59F28");

/// Outcome tag on a step whose action was delivered.
///
/// Minted with `trible genid` on 2026-11-11:
/// `30CC8AC8BBC08D28FDA4DF152E824DFE`.
pub const KIND_STEP_DELIVERED: Id = id_hex!("30CC8AC8BBC08D28FDA4DF152E824DFE");

/// Outcome tag on a step whose action failed (its error is the step's
/// `metadata::description`). The ladder goes on regardless.
///
/// Minted with `trible genid` on 2026-11-11:
/// `438B08CE4F8B32C82872D186E76874B1`.
pub const KIND_STEP_FAILED: Id = id_hex!("438B08CE4F8B32C82872D186E76874B1");

/// Tag on a test raise: it never goes past R0 and never mentions JP.
///
/// Minted with `trible genid` on 2026-11-11:
/// `B481E6D1AF08C281354359E9859BF72C`.
pub const KIND_TEST: Id = id_hex!("B481E6D1AF08C281354359E9859BF72C");

/// Tag on a resolve that says the alarm should not have been raised. It
/// halves the raiser's daily allowance for seven days.
///
/// Minted with `trible genid` on 2026-11-11:
/// `622345A4D7BCD5584DE7E2C8D7AB071E`.
pub const KIND_FALSE_ALARM: Id = id_hex!("622345A4D7BCD5584DE7E2C8D7AB071E");

/// Class: data is being lost (a truncation, a mass deletion).
///
/// Minted with `trible genid` on 2026-11-11:
/// `F30BAA9AE12E21BDC9C0A49B4DC890F5`.
pub const CLASS_DATA_LOSS: Id = id_hex!("F30BAA9AE12E21BDC9C0A49B4DC890F5");

/// Class: a recovery window is decaying (a snapshot about to cycle out).
///
/// Minted with `trible genid` on 2026-11-11:
/// `2FEAC5443995B3CF5BEA937C30B8C99E`.
pub const CLASS_RECOVERY_WINDOW: Id = id_hex!("2FEAC5443995B3CF5BEA937C30B8C99E");

/// Class: a host is hung and needs hands.
///
/// Minted with `trible genid` on 2026-11-11:
/// `4BAC7221E2B42150B7DD9DCB7E5C9ECB`.
pub const CLASS_HUNG_HOST: Id = id_hex!("4BAC7221E2B42150B7DD9DCB7E5C9ECB");

/// Class: a security incident.
///
/// Minted with `trible genid` on 2026-11-11:
/// `19902F164719EF63D47CA331879E133A`.
pub const CLASS_SECURITY: Id = id_hex!("19902F164719EF63D47CA331879E133A");

/// The classes this build raises, with the name each is written under
/// (`metadata::name` on the class entity) and accepted as on the CLI.
///
/// Writing is closed over this table; reading is not. A class minted by a
/// newer build lists by the name the pile carries for it, or by its id.
pub const CLASSES: [(Id, &str); 4] = [
    (CLASS_DATA_LOSS, "data-loss"),
    (CLASS_RECOVERY_WINDOW, "recovery-window"),
    (CLASS_HUNG_HOST, "hung-host"),
    (CLASS_SECURITY, "security"),
];

/// Rung: a message to the colony broadcast group (every zooid's watcher).
///
/// Minted with `trible genid` on 2026-11-11:
/// `F4E6FCC62A5C14921D44DBD2A508C545`.
pub const RUNG_COLONY: Id = id_hex!("F4E6FCC62A5C14921D44DBD2A508C545");

/// Rung: a Discord line mentioning JP (R0 posts it, later mentions reply to
/// it).
///
/// Minted with `trible genid` on 2026-11-11:
/// `94AE05554D4C2470A9625D0F7FC58112`.
pub const RUNG_DISCORD_MENTION: Id = id_hex!("94AE05554D4C2470A9625D0F7FC58112");

/// Rung: a say request in the Discord voice channel, only while JP is in it.
///
/// Minted with `trible genid` on 2026-11-11:
/// `2445FE1A93253B871E512173C73AAEBD`.
pub const RUNG_DISCORD_VOICE: Id = id_hex!("2445FE1A93253B871E512173C73AAEBD");

/// Rung: a notification banner on the Mac.
///
/// Minted with `trible genid` on 2026-11-11:
/// `CA5596B34C32DA4DEE63B076FF8D6E27`.
pub const RUNG_MAC_NOTIFY: Id = id_hex!("CA5596B34C32DA4DEE63B076FF8D6E27");

/// Rung: a loud sound on the Mac, volume restored afterwards.
///
/// Minted with `trible genid` on 2026-11-11:
/// `22998FE7FFB6159EEB87E03ABAC5D3F5`.
pub const RUNG_MAC_SOUND: Id = id_hex!("22998FE7FFB6159EEB87E03ABAC5D3F5");

/// Rung: `voice shout` on the Mac (its route policy ends in Reachy).
///
/// Minted with `trible genid` on 2026-11-11:
/// `A814C945124580BFEC2315E1B8143B86`.
pub const RUNG_REACHY_SHOUT: Id = id_hex!("A814C945124580BFEC2315E1B8143B86");

/// Rung: a Hue alert on JP's lights.
///
/// Minted with `trible genid` on 2026-11-11:
/// `F867BB343CE049F3BEA072687589CEA9`.
pub const RUNG_HUE_ALERT: Id = id_hex!("F867BB343CE049F3BEA072687589CEA9");

/// Rung: the final Discord line when the ladder stops sounding ("still
/// unacknowledged; stopped sounding; alarm stays open"). Not in the design's
/// rung list, which names the final line without giving it a rung; without
/// one the ladder could not tell it was already sent.
///
/// Minted with `trible genid` on 2026-11-11:
/// `3BE15BB9D7E0C8954050B924399E12FF`.
pub const RUNG_FINAL_DISCORD: Id = id_hex!("3BE15BB9D7E0C8954050B924399E12FF");

/// Rung: the final colony message when the ladder stops sounding.
///
/// Minted with `trible genid` on 2026-11-11:
/// `58D8A3B2EB98BE2A24BD7EB17A4DFCB5`.
pub const RUNG_FINAL_COLONY: Id = id_hex!("58D8A3B2EB98BE2A24BD7EB17A4DFCB5");

/// The rungs this build runs, with the name each is written under.
pub const RUNGS: [(Id, &str); 9] = [
    (RUNG_COLONY, "colony"),
    (RUNG_DISCORD_MENTION, "discord-mention"),
    (RUNG_DISCORD_VOICE, "discord-voice"),
    (RUNG_MAC_NOTIFY, "mac-notify"),
    (RUNG_MAC_SOUND, "mac-sound"),
    (RUNG_REACHY_SHOUT, "reachy-shout"),
    (RUNG_HUE_ALERT, "hue-alert"),
    (RUNG_FINAL_DISCORD, "final-discord"),
    (RUNG_FINAL_COLONY, "final-colony"),
];

/// Via: `alarm ack` by the person who saw it.
///
/// Minted with `trible genid` on 2026-11-11:
/// `0ACF79419247D63ED199602D7407838F`.
pub const VIA_CLI: Id = id_hex!("0ACF79419247D63ED199602D7407838F");

/// Via: a zooid relaying JP's word (the ack's `message::local::from` is the
/// relayer, its description says where JP said it).
///
/// Minted with `trible genid` on 2026-11-11:
/// `E9F88F3150934517FFC4FFC207C229F4`.
pub const VIA_RELAYED: Id = id_hex!("E9F88F3150934517FFC4FFC207C229F4");

/// Via: JP replied on Discord to a message the alarm posted.
///
/// Minted with `trible genid` on 2026-11-11:
/// `0B6EC3EACCAB8B57E3316F4F6473F5CF`.
pub const VIA_DISCORD_REPLY: Id = id_hex!("0B6EC3EACCAB8B57E3316F4F6473F5CF");

/// Via: a short whitelisted JP line on Discord after the raise ("seen").
///
/// Minted with `trible genid` on 2026-11-11:
/// `006D7F135067837D9D10BCB54303BE22`.
pub const VIA_DISCORD_LINE: Id = id_hex!("006D7F135067837D9D10BCB54303BE22");

/// Via: JP said it in the voice channel (a heard utterance).
///
/// Minted with `trible genid` on 2026-11-11:
/// `15EA7DFF6E70293D2BC151833518222E`.
pub const VIA_VOICE: Id = id_hex!("15EA7DFF6E70293D2BC151833518222E");

/// The ack channels this build writes, with the name each is written under.
pub const VIAS: [(Id, &str); 5] = [
    (VIA_CLI, "cli"),
    (VIA_RELAYED, "relayed"),
    (VIA_DISCORD_REPLY, "discord-reply"),
    (VIA_DISCORD_LINE, "discord-line"),
    (VIA_VOICE, "voice"),
];

pub mod alarm {
    use super::*;
    attributes! {
        /// The alarm an ack, resolve, note or step record is about.
        ///
        /// Anchor minted with `trible genid` on 2026-11-11.
        "B5112C6E9E5B35390A83A9EBA15C7B08" as of: inlineencodings::GenId;
        /// What is lost, and by when, if nobody acts. Required on a raise:
        /// "I'm unsure" has no decay to name.
        ///
        /// Anchor minted with `trible genid` on 2026-11-11.
        "5EC366D40F532BBBF6720F5CFE8A8A16" as decays: inlineencodings::Handle<blobencodings::UTF8String>;
        /// What the raiser measured. Required on a raise.
        ///
        /// Anchor minted with `trible genid` on 2026-11-11.
        "6803150733FCCF4ED74C5DB5FD258F47" as evidence: inlineencodings::Handle<blobencodings::UTF8String>;
        /// Optional deduplication key, e.g. `pile-truncation:sky:self.pile`.
        ///
        /// Anchor minted with `trible genid` on 2026-11-11.
        "F03582ABE7F7DA524B8B6FDDF998A349" as incident: inlineencodings::Handle<blobencodings::UTF8String>;
        /// Which rung a step is ([`RUNGS`]).
        ///
        /// Anchor minted with `trible genid` on 2026-11-11.
        "A03F0C6FC1970C9AC43412AA9F80DB27" as rung: inlineencodings::GenId;
        /// How an ack arrived ([`VIAS`]).
        ///
        /// Anchor minted with `trible genid` on 2026-11-11.
        "9227ABCD0C61D429BFF8E114CAAD24CB" as via: inlineencodings::GenId;
        /// Whom the alarm is for: a Relations person (always JP in v1).
        /// The design calls it `alarm::for`, a Rust keyword.
        ///
        /// Anchor minted with `trible genid` on 2026-11-11.
        "8BEB5DE13A0547E08D9C2776A103A1CB" as for_person: inlineencodings::GenId;
    }
}
