//! Discord schema: stable upstream anchors, immutable semantic observations,
//! attachment occurrences, and explicit ingestion coverage.
//!
//! Used by `discord.rs` (the faculty CLI). Message observations use the generic
//! `archive::*` schema for their common projection (author / content / reply_to
//! / kind_message); this module owns only Discord-specific identity and
//! context. Attachment occurrences link to canonical file records built with
//! the shared `faculties::files` model.
//!
//! ## Identity
//!
//! Discord snowflakes are globally unique upstream, but mutable Discord
//! resources must not become mutable entities in a union-only collection. A
//! message snowflake therefore derives a stable identity anchor carrying only
//! [`discord::message_id`]. Each semantic version is a separate intrinsic
//! observation linked to that anchor through [`discord::message`]. Volatile
//! REST response state is deliberately absent. Discord users follow the same
//! pattern: messages point to stable user anchors, while names are independent
//! profile observations.
//!
//! Coverage is an integer interval `(after_exclusive, through_inclusive]`, not
//! a scalar high-water mark. Baseline intervals make the intentionally bounded
//! first import explicit; ordinary intervals may advance a reader only when
//! they connect to that baseline cover.
//!
//! Something said in a voice channel is a message there too: its anchor is
//! the utterance itself ([`discord::kind_utterance`]: who spoke, in which
//! channel, from when), since no snowflake names it, and its observation
//! carries the transcript as content and the audio beside it
//! ([`discord::utterance_audio`]). A sentence finished while the speaker
//! went on is published before the utterance closes, as a record of its own
//! that links the utterance and is no message ([`discord::kind_heard_sentence`]).
//!
//! Somebody coming into or leaving that voice channel is a system notice
//! there: a message by them with no content, marked as a notice, whose
//! anchor says which it is ([`discord::kind_voice_joined`],
//! [`discord::kind_voice_left`]: who, in which channel, seen when).
//!
//! Coverage says what is stored, not what was heard: `discord read` brings a
//! channel's history in with the same observations `discord live`'s intake
//! stores. Where a channel's live intake begins is therefore a fact of its
//! own ([`discord::kind_intake`]), which is how a reader looking for news
//! tells what was sent while the channel was heard from its history.

use triblespace::macros::id_hex;
use triblespace::prelude::blobencodings::{RawBytes, UTF8String};
use triblespace::prelude::inlineencodings::{GenId, Handle, NsTAIInterval, U256BE};
use triblespace::prelude::*;

/// Stable extrinsic scope of the Discord observation collection.
///
/// Minted with `trible genid` on 2026-08-07:
/// `908A81B67AF50D568C3863E7D6708EEB`.
pub const DEFAULT_SCOPE_ID: Id = id_hex!("908A81B67AF50D568C3863E7D6708EEB");

pub mod discord {
    use super::*;

    attributes! {
        /// Exact encrypted Secrets version used by an authentication revision.
        /// Minted with `trible genid` on 2026-10-10.
        "79C7FA989CF7A5419C3CEA7153101203" as pub auth_secret_version: GenId;
        /// Link from a channel entity to its parent guild.
        "E3022EC14FD000BB8556CD32C2C68E59" unsafe as pub guild: GenId;
        /// Link from a message entity to its channel.
        "B8EA57CD650A678ACA5D1479BF195C4C" unsafe as pub channel: GenId;
        /// Link from an immutable message observation to the stable identity
        /// anchor derived from the upstream Discord message snowflake.
        ///
        /// Minted with `trible genid` on 2026-08-07:
        /// `4B9C024EDD627A4E8786E01B196FDF16`.
        "4B9C024EDD627A4E8786E01B196FDF16" as pub message: GenId;
        /// Link from an immutable Discord profile observation to its stable
        /// Discord user anchor.
        ///
        /// Minted with `trible genid` on 2026-08-08:
        /// `CE2A12E5A260253138C86DD2D15654C7`.
        "CE2A12E5A260253138C86DD2D15654C7" as pub user: GenId;
        /// External Discord snowflake for a guild (server). Stored
        /// as a string — Discord ids are u64 but the REST API
        /// ships them as strings to survive JavaScript clients.
        "9E8EC81F5C14805CCFD4930A4B877138" unsafe as pub guild_id: Handle<UTF8String>;
        /// External Discord snowflake for a channel.
        "7C943A11E09C922989CAFE22B92E9A51" unsafe as pub channel_id: Handle<UTF8String>;
        /// External Discord snowflake for a message.
        "758C42164B566C2AFECBCD7129163A34" unsafe as pub message_id: Handle<UTF8String>;
        /// External Discord snowflake for a user.
        "2A74F35C6720A0C60BF43D30DF272F85" unsafe as pub user_id: Handle<UTF8String>;
        /// Full Discord JSON body of a message. Stored raw so
        /// future code can derive additional fields without
        /// re-fetching.
        #[allow(dead_code)]
        "5B9DCF6170CD775FC5DA22C8DB96599D" unsafe as pub message_raw: Handle<UTF8String>;
        /// Bot token (passed to the REST API as `Authorization:
        /// Bot <token>`). One token per bot identity; a caller
        /// who operates multiple bots would tag the token entity
        /// with a different `kind` or a user-scoped id.
        #[allow(dead_code)]
        "E20FEC3E1714D5EDC556936AE1C0F463" unsafe as pub bot_token: Handle<UTF8String>;
        /// Per-channel pagination cursor — the snowflake of the
        /// newest message we ingested. Next sync fetches
        /// `?after=<cursor>`. Stored as a UTF8String handle for
        /// consistency with the other snowflake attributes.
        #[allow(dead_code)]
        "3C510E125ACE09DC9B297D533C0F13B7" unsafe as pub cursor_last_message_id: Handle<UTF8String>;
        /// Exclusive lower endpoint of one fully persisted numeric coverage
        /// interval.
        ///
        /// Minted with `trible genid` on 2026-08-08:
        /// `A37BBC85528AF14B6C20280886B7A537`.
        "A37BBC85528AF14B6C20280886B7A537" as pub receipt_after_exclusive: U256BE;
        /// Inclusive upper endpoint of one fully persisted numeric coverage
        /// interval.
        ///
        /// Minted with `trible genid` on 2026-08-07:
        /// `8B9F2C90AB42911696E17F49974CD28B`.
        "8B9F2C90AB42911696E17F49974CD28B" as pub receipt_through_inclusive: U256BE;
        /// The moment from which a channel's live intake hears it: what is
        /// sent in the channel at or after it was heard, whoever stored it,
        /// and what was sent before it is the channel's history. Discord's
        /// millisecond, as a point interval like a message's
        /// `metadata::created_at`.
        ///
        /// Minted with `trible genid` on 2026-09-26:
        /// `066E9645CB6045F63AA0E9BFBC064337`.
        "066E9645CB6045F63AA0E9BFBC064337" as pub heard_from: NsTAIInterval;
        /// The audio of a spoken utterance's observation ([`kind_utterance`]):
        /// what was said, as a 16 kHz mono 16-bit PCM WAV, beside the
        /// transcript in `archive::content`.
        ///
        /// Minted with `trible genid` on 2026-10-05:
        /// `C86AF50A0542619BE20A22EFFBE15033`.
        "C86AF50A0542619BE20A22EFFBE15033" as pub utterance_audio: Handle<RawBytes>;
    }

    /// Root id for describing the Discord protocol in metadata.
    #[allow(non_upper_case_globals)]
    #[allow(dead_code)]
    pub const discord_metadata: Id = id_hex!("2D7920FB46B6821912F51371BF1FB4FE");

    /// An immutable bot authentication revision. Successors explicitly name
    /// the revisions they observed through `metadata::supersedes`.
    /// Minted with `trible genid` on 2026-10-10.
    #[allow(non_upper_case_globals)]
    pub const kind_auth: Id = id_hex!("E5B4A5CD56EB07D6E02661D2C0D4D761");

    /// Tag for Discord guild (server) entities.
    #[allow(non_upper_case_globals)]
    pub const kind_guild: Id = id_hex!("6D2F005AEAE95696708C50DDE1E09BED");
    /// Tag for Discord channel entities.
    #[allow(non_upper_case_globals)]
    pub const kind_channel: Id = id_hex!("7812454E8EFBB87245AE770B48EFC611");
    /// Tag for stable Discord user anchors.
    ///
    /// Minted with `trible genid` on 2026-08-08:
    /// `3548F7AC7E229BCFAD1347FEC256C25C`.
    #[allow(non_upper_case_globals)]
    pub const kind_user: Id = id_hex!("3548F7AC7E229BCFAD1347FEC256C25C");
    /// Tag for immutable observed Discord user profiles.
    ///
    /// Minted with `trible genid` on 2026-08-08:
    /// `B1751751B2DDFDF166D4EF0DA8D53D13`.
    #[allow(non_upper_case_globals)]
    pub const kind_user_profile: Id = id_hex!("B1751751B2DDFDF166D4EF0DA8D53D13");
    /// Tag for a fully successful forward-ingestion interval.
    ///
    /// Minted with `trible genid` on 2026-08-07:
    /// `EB592647E59EBBF07E7221DDC746A2B6`.
    #[allow(non_upper_case_globals)]
    pub const kind_ingestion_receipt: Id = id_hex!("EB592647E59EBBF07E7221DDC746A2B6");
    /// Tag for the explicit bounded-history baseline established by a first
    /// import. Older snowflakes are intentionally outside its claim.
    ///
    /// Minted with `trible genid` on 2026-08-08:
    /// `D5CE5556F159FE3F34A67A87DB105281`.
    #[allow(non_upper_case_globals)]
    pub const kind_ingestion_baseline: Id = id_hex!("D5CE5556F159FE3F34A67A87DB105281");
    /// Tag for the Discord user a bot token of this pile authenticates as,
    /// linked through [`user`]. What that user writes is the pile's own, so a
    /// reader looking for news from others passes over it.
    ///
    /// Minted with `trible genid` on 2026-09-26:
    /// `F131FEA2CAF2573F40AEC8F4C68D85DA`.
    #[allow(non_upper_case_globals)]
    pub const kind_bot_account: Id = id_hex!("F131FEA2CAF2573F40AEC8F4C68D85DA");
    /// Tag for a Discord message that is a system notice rather than
    /// something somebody wrote (a pin, a member joining, a boost, a thread
    /// starting), linked through [`message`] to its anchor. Discord gives
    /// such messages an author, so this is how a reader looking for people
    /// writing passes over them.
    ///
    /// Minted with `trible genid` on 2026-09-26:
    /// `B0D2317A9B8F77AFB906CC35EB5AB516`.
    #[allow(non_upper_case_globals)]
    pub const kind_system_notice: Id = id_hex!("B0D2317A9B8F77AFB906CC35EB5AB516");
    /// Tag for where a channel's live intake begins, linked through
    /// [`channel`] and dated by [`heard_from`]. `discord live` records it
    /// with what it stores in the channel, and on its own when a backfill
    /// stores nothing; a DM channel whose beginning moves earlier has one
    /// for each, and the earliest is where it begins. A message sent before
    /// every one of them, or in a channel without one, is history, however
    /// it came in.
    ///
    /// Minted with `trible genid` on 2026-09-26:
    /// `C6AF15CBF07E7E6537752837DE056917`.
    #[allow(non_upper_case_globals)]
    pub const kind_intake: Id = id_hex!("C6AF15CBF07E7E6537752837DE056917");
    /// Tag for the stable anchor of something said in a voice channel: who
    /// ([`user`]) spoke in which [`channel`], from when
    /// (`metadata::created_at`). It is a message anchor like one a snowflake
    /// derives, linked from its observations through [`message`], but no
    /// Discord message id names it, so it carries no [`message_id`].
    ///
    /// Minted with `trible genid` on 2026-10-05:
    /// `3D20B9F91AE85C3778B71123578F1599`.
    #[allow(non_upper_case_globals)]
    pub const kind_utterance: Id = id_hex!("3D20B9F91AE85C3778B71123578F1599");
    /// Tag for a sentence somebody finished in a voice channel while they
    /// went on speaking: the hearing's transcript had moved past its end,
    /// so it is published before the utterance it belongs to closes. It
    /// links that utterance's anchor through [`message`] and carries the
    /// sentence as `archive::content`, its speaker as `archive::author`, its
    /// [`channel`], and as `metadata::created_at` when the hearing had heard
    /// it (the end of the audio whose transcript showed it finished; not
    /// when the sentence itself ended, which the transcriber does not say).
    /// It is not a message: no audio, no `archive::kind_message`, so a
    /// reader of messages never sees it. The utterance, once stored, holds
    /// the whole transcript and audio, this sentence included; a reader
    /// looking for news takes the sentence for news only until then.
    ///
    /// Minted with `trible genid` on 2026-10-10:
    /// `1990A670956927EEC6C9C45D53B96B47`.
    #[allow(non_upper_case_globals)]
    pub const kind_heard_sentence: Id = id_hex!("1990A670956927EEC6C9C45D53B96B47");
    /// Tag for the stable anchor of somebody coming into a voice channel:
    /// who ([`user`]) came into which [`channel`], seen when
    /// (`metadata::created_at`). Like an utterance's, it is a message anchor
    /// no Discord message id names. Its observation is a message by that
    /// user with no content, marked a system notice ([`kind_system_notice`]),
    /// so a reader that does not know this tag passes it over as it passes
    /// over a member joining the server, and one that does presents it as
    /// the event it is, never as something the member said or wrote.
    ///
    /// Minted with `trible genid` on 2026-10-06:
    /// `F4134A58DF84885EF016676BE95925B5`.
    #[allow(non_upper_case_globals)]
    pub const kind_voice_joined: Id = id_hex!("F4134A58DF84885EF016676BE95925B5");
    /// Tag for the stable anchor of somebody leaving a voice channel, as
    /// [`kind_voice_joined`] is for coming into one.
    ///
    /// Minted with `trible genid` on 2026-10-06:
    /// `9C753182D87C69EBC51E7265358D98C3`.
    #[allow(non_upper_case_globals)]
    pub const kind_voice_left: Id = id_hex!("9C753182D87C69EBC51E7265358D98C3");
}
