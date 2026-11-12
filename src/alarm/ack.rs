//! Acks detected from Discord facts the pile already holds.
//!
//! Two things JP does on Discord ack an alarm:
//!
//! - He REPLIES to a message the alarm posted. A reply's `archive::reply_to`
//!   names the replied message's anchor, and the anchor carries its
//!   snowflake (`discord::message_id`); every step that posted a Discord
//!   message carries the same snowflake. The two are joined on that VALUE:
//!   no anchor id is ever computed to look one up.
//! - He writes a short line after the raise, in any channel or DM the pile
//!   holds, whose whole text is one of [`ACK_WORDS`] or the alarm's id8. A
//!   heard utterance is stored as a message in the voice channel under its
//!   speaker, so a spoken "seen" arrives the same way, as [`VIA_VOICE`].
//!   Any other line of his does not ack: he may be talking about something
//!   else.
//!
//! JP's lines are recognised by his Discord user snowflake, which is
//! configuration: a user anchor carrying it (`discord::user_id`) is him,
//! unless the pile marks that user as its own bot account. Nothing here is
//! a write; the caller records what it found as an ack.

use triblespace::core::inline::encodings::time::Lower;
use triblespace::core::metadata;
use triblespace::core::query::rangeconstraint::value_range;
use triblespace::core::query::TriblePattern;
use triblespace::core::repo::BlobStoreGet;
use triblespace::prelude::*;

use super::guards::lower_after;
use super::operations::read_text;
use super::{text_handle, TextHandle};
use crate::schemas::alarm::{
    alarm, KIND_ALARM, KIND_STEP, VIA_DISCORD_LINE, VIA_DISCORD_REPLY, VIA_VOICE,
};
use crate::schemas::archive::archive;
use crate::schemas::discord::discord;

/// Whole lines that ack an alarm, after [`normalize`].
pub const ACK_WORDS: &[&str] = &[
    "seen", "seen it", "i see it", "ack", "ok", "okay", "got it", "on it",
];

/// An ack found in the Discord facts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Detected {
    /// [`VIA_DISCORD_REPLY`], [`VIA_DISCORD_LINE`] or [`VIA_VOICE`].
    pub via: Id,
    /// The anchor of JP's message that acked.
    pub message: Id,
    /// When he sent or said it, TAI nanoseconds.
    pub at: i128,
}

/// Lowercase, trimmed of surrounding punctuation, inner whitespace collapsed:
/// "Seen it!" and " seen   it. " are both "seen it".
pub fn normalize(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_matches(|c: char| c.is_ascii_punctuation() || c.is_whitespace())
        .to_lowercase()
}

/// Whether a whole line acks the alarm whose short id is `id8`.
pub fn acks(text: &str, id8: &str) -> bool {
    let line = normalize(text);
    ACK_WORDS.contains(&line.as_str()) || line == id8.to_lowercase()
}

/// The user anchors that are JP: those carrying his snowflake, less any the
/// pile marks as its own bot account.
fn jp_users<D>(discord_facts: &D, jp_user: &str) -> Vec<Id>
where
    D: TriblePattern + ?Sized,
{
    let snowflake = text_handle(jp_user.trim());
    find!(
        user: Id,
        pattern!(discord_facts, [{ ?user @ discord::user_id: snowflake }])
    )
    .filter(|user| {
        let user = *user;
        !exists!(pattern!(discord_facts, [{ _?account @
            metadata::tag: discord::kind_bot_account,
            discord::user: user,
        }]))
    })
    .collect()
}

/// The earliest ack of `alarm` JP left in `discord_facts`, if any. `reader`
/// holds the message texts; a line whose text is not here does not ack yet.
pub fn detect<A, D, R>(
    alarm_facts: &A,
    discord_facts: &D,
    reader: &R,
    alarm_id: Id,
    jp_user: &str,
) -> Option<Detected>
where
    A: TriblePattern + ?Sized,
    D: TriblePattern + ?Sized,
    R: BlobStoreGet + ?Sized,
{
    if jp_user.trim().is_empty() {
        return None;
    }
    let raised_at = find!(
        at: Lower,
        pattern!(alarm_facts, [{ alarm_id @ metadata::tag: &KIND_ALARM, metadata::created_at: ?at }])
    )
    .map(|at| at.0)
    .min()?;
    let id8 = format!("{alarm_id:x}")[..8].to_owned();
    let mut found: Vec<(i128, u8, Detected)> = Vec::new();
    for user in jp_users(discord_facts, jp_user) {
        // Replies to a message the alarm posted, joined on its snowflake.
        for (message, at) in find!(
            (message: Id, at: Lower),
            temp!((observation, target, snowflake), and!(
                pattern!(discord_facts, [
                    { ?observation @
                        metadata::tag: archive::kind_message,
                        discord::message: ?message,
                        archive::author: user,
                        archive::reply_to: ?target,
                        metadata::created_at: ?at,
                    },
                    { ?target @ discord::message_id: ?snowflake },
                ]),
                pattern!(alarm_facts, [{ _?step @
                    metadata::tag: &KIND_STEP,
                    alarm::of: alarm_id,
                    discord::message_id: ?snowflake,
                }]),
            ))
        ) {
            found.push((
                at.0,
                0,
                Detected {
                    via: VIA_DISCORD_REPLY,
                    message,
                    at: at.0,
                },
            ));
        }
        // Short lines after the raise.
        let (min, max) = lower_after(raised_at, None);
        for (message, content, at) in find!(
            (message: Id, content: TextHandle, at: Lower),
            temp!((observation), and!(
                pattern!(discord_facts, [{ ?observation @
                    metadata::tag: archive::kind_message,
                    discord::message: ?message,
                    archive::author: user,
                    archive::content: ?content,
                    metadata::created_at: ?at,
                }]),
                value_range(at, min, max),
            ))
        ) {
            if !read_text(reader, content).is_some_and(|text| acks(&text, &id8)) {
                continue;
            }
            let spoken = exists!(
                pattern!(discord_facts, [{ message @ metadata::tag: discord::kind_utterance }])
            );
            found.push((
                at.0,
                1,
                Detected {
                    via: if spoken { VIA_VOICE } else { VIA_DISCORD_LINE },
                    message,
                    at: at.0,
                },
            ));
        }
    }
    found.sort_by_key(|(at, rank, detected)| (*at, *rank, detected.message));
    found.into_iter().next().map(|(_, _, detected)| detected)
}

/// Whether JP is in a voice channel now: the newest time he was seen
/// joining one is later than the newest time he was seen leaving one.
pub fn jp_in_voice<D>(discord_facts: &D, jp_user: &str) -> bool
where
    D: TriblePattern + ?Sized,
{
    if jp_user.trim().is_empty() {
        return false;
    }
    let mut joined = None;
    let mut left = None;
    for user in jp_users(discord_facts, jp_user) {
        let newest = |tag: Id| {
            find!(
                at: Lower,
                pattern!(discord_facts, [{ _?notice @
                    metadata::tag: tag,
                    discord::user: user,
                    metadata::created_at: ?at,
                }])
            )
            .map(|at| at.0)
            .max()
        };
        joined = joined.max(newest(discord::kind_voice_joined));
        left = left.max(newest(discord::kind_voice_left));
    }
    match (joined, left) {
        (Some(joined), Some(left)) => joined > left,
        (Some(_), None) => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests;
