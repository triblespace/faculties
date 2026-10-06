//! Who comes into and leaves `discord live`'s voice channel, told apart from
//! everything else a member's voice state says.
//!
//! The gateway session holds the guild's voice states (GUILD_VOICE_STATES),
//! so Discord hands it a VOICE_STATE_UPDATE for every member of the guild
//! whenever anything about their voice changes: muting, deafening, a stream,
//! a camera, or the channel they are in. Only the channel matters here, and
//! only whether it is the bot's: a member whose channel becomes the bot's has
//! joined it, and one whose channel stops being it (another channel, or none)
//! has left. Every other update is not a change of who is there.
//!
//! An update says where a member is now, not where they were, so telling a
//! join from a mute needs to know who was there: the guild's GUILD_CREATE
//! lists every voice state, after the first READY and after every later one
//! (a new session, which knows nothing of the last one's). It is compared
//! with what the process knew before it. At the process's start that is
//! nothing, so whoever is in the channel when the bot arrives is seen coming
//! in then: a bot restarted while somebody sits in the channel must learn
//! they are there, and so must whatever pauses for them. After a new
//! session it is what the old one knew, so a member who came or went while
//! there was no session is seen at the new session's GUILD_CREATE. A RESUMED
//! session keeps what it knew, and Discord replays what it missed meanwhile,
//! so a member who came or went while it was away is seen then, as it
//! reaches the bot. Between a READY and its GUILD_CREATE nobody's coming or
//! going can be told, and nothing is a change.
//!
//! A member is there with one voice session, and a client of theirs that
//! comes back under a new one takes the member's place in the channel over:
//! the end of the older session, which can be reported after the newer one
//! began, is not their leaving, as it is not the bot's own (the voice
//! connection's `VoiceJoin` passes it over the same way). Only the end of
//! the session they are there with is, or an update that names none.
//!
//! A change is seen when it reaches the bot, which Discord's replay to a
//! resumed session does in one burst: each change is therefore seen at
//! least a millisecond after the one before it, so that every change is a
//! notice of its own (a notice is who, where and when, and two alike at one
//! time would be one) and the notices are in the order the changes were.
//!
//! What is known of who is there is the process's own, held only to tell
//! the edges apart; the collection stores the changes, never a list of
//! members ([`crate::discord::presence_fragment`]). The bot's own comings
//! and goings, which the voice connection handles, are never a change.

use std::collections::BTreeMap;
use std::num::NonZeroU64;

use serde_json::Value;

use super::operations::PresenceChange;
use crate::discord::Presence;

/// Who is in one voice channel of one guild, as far as the gateway has said.
#[derive(Debug)]
pub struct Members {
    guild: u64,
    channel: u64,
    /// The bot's own user, from READY.
    bot: Option<u64>,
    /// Who is in the channel, with the voice session each is there with
    /// when Discord named it, once the guild's GUILD_CREATE said who was.
    present: Option<BTreeMap<u64, Option<String>>>,
    /// What the last session knew of who is in the channel, kept across a
    /// READY for the next GUILD_CREATE to be compared with. Empty at the
    /// process's start.
    remembered: BTreeMap<u64, Option<String>>,
    /// When the last change was seen, in milliseconds since the Unix epoch.
    last_seen_ms: u64,
}

impl Members {
    pub fn new(guild: NonZeroU64, channel: NonZeroU64) -> Self {
        Self {
            guild: guild.get(),
            channel: channel.get(),
            bot: None,
            present: None,
            remembered: BTreeMap::new(),
            last_seen_ms: 0,
        }
    }

    /// The changes of who is in the channel that `dispatch` makes, in the
    /// order they are seen, each seen at `now_ms` (milliseconds since the
    /// Unix epoch), or a millisecond after the change before it when that is
    /// later. Usually none or one; a GUILD_CREATE makes one for every member
    /// it lists in the channel that the process did not know was there, and
    /// one for every member it knew that is gone.
    pub fn observe(&mut self, dispatch: &Value, now_ms: u64) -> Vec<PresenceChange> {
        self.changes(dispatch, now_ms).unwrap_or_default()
    }

    fn changes(&mut self, dispatch: &Value, now_ms: u64) -> Option<Vec<PresenceChange>> {
        let d = &dispatch["d"];
        match dispatch["t"].as_str()? {
            "READY" => {
                // A new session: what the last one knew is remembered for
                // its GUILD_CREATE to be compared with, and nothing is a
                // change until then.
                self.bot = snowflake(&d["user"]["id"]);
                if let Some(present) = self.present.take() {
                    self.remembered = present;
                }
                None
            }
            "GUILD_CREATE" if snowflake(&d["id"]) == Some(self.guild) => {
                let states = d["voice_states"].as_array()?;
                let before = self
                    .present
                    .take()
                    .unwrap_or_else(|| std::mem::take(&mut self.remembered));
                let mut present = BTreeMap::new();
                let mut changes = Vec::new();
                for state in states {
                    if snowflake(&state["channel_id"]) != Some(self.channel) {
                        continue;
                    }
                    let Some(user) = snowflake(&state["user_id"]) else {
                        continue;
                    };
                    if Some(user) == self.bot {
                        continue;
                    }
                    if !before.contains_key(&user) {
                        let name = name(&state["member"]["user"]);
                        changes.push(self.change(user, name, Presence::Joined, now_ms));
                    }
                    present.insert(user, voice_session(state));
                }
                for user in before.keys() {
                    if !present.contains_key(user) {
                        changes.push(self.change(*user, None, Presence::Left, now_ms));
                    }
                }
                self.present = Some(present);
                Some(changes)
            }
            "VOICE_STATE_UPDATE" if snowflake(&d["guild_id"]) == Some(self.guild) => {
                let user = snowflake(&d["user_id"])?;
                if Some(user) == self.bot {
                    return None;
                }
                let present = self.present.as_mut()?;
                let here = snowflake(&d["channel_id"]) == Some(self.channel);
                let session = voice_session(d);
                let presence = match (present.get_mut(&user), here) {
                    (None, true) => {
                        present.insert(user, session);
                        Presence::Joined
                    }
                    // Still there, perhaps with a new voice session.
                    (Some(known), true) => {
                        if session.is_some() {
                            *known = session;
                        }
                        return None;
                    }
                    // The end of a voice session the member is not there
                    // with: an older one, which their newer one replaced.
                    (Some(Some(known)), false)
                        if session.as_ref().is_some_and(|session| *session != *known) =>
                    {
                        return None
                    }
                    (Some(_), false) => {
                        present.remove(&user);
                        Presence::Left
                    }
                    // Still elsewhere.
                    (None, false) => return None,
                };
                let name = name(&d["member"]["user"]);
                Some(vec![self.change(user, name, presence, now_ms)])
            }
            _ => None,
        }
    }

    /// One change, seen at `now_ms` or a millisecond after the last.
    fn change(
        &mut self,
        user: u64,
        name: Option<String>,
        presence: Presence,
        now_ms: u64,
    ) -> PresenceChange {
        let seen_ms = now_ms.max(self.last_seen_ms.saturating_add(1));
        self.last_seen_ms = seen_ms;
        PresenceChange {
            channel: self.channel,
            user,
            name,
            presence,
            seen_ms,
        }
    }
}

/// A user's name as a message author's is taken: the global name, else the
/// user name.
fn name(user: &Value) -> Option<String> {
    user["global_name"]
        .as_str()
        .filter(|name| !name.is_empty())
        .or_else(|| user["username"].as_str())
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
}

/// The voice session a voice state belongs to, when it names one.
fn voice_session(state: &Value) -> Option<String> {
    state["session_id"].as_str().map(str::to_owned)
}

fn snowflake(value: &Value) -> Option<u64> {
    value.as_str().and_then(|id| id.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const GUILD: u64 = 100000000000000300;
    const VOICE: u64 = 100000000000000201;
    const ELSEWHERE: u64 = 100000000000000202;
    const BOT: u64 = 100000000000000900;
    const ADA: u64 = 100000000000000400;
    const GRACE: u64 = 100000000000000401;
    /// No change at all.
    const NONE: [(usize, u64, Presence); 0] = [];

    fn members() -> Members {
        Members::new(
            NonZeroU64::new(GUILD).unwrap(),
            NonZeroU64::new(VOICE).unwrap(),
        )
    }

    fn ready() -> Value {
        json!({"t": "READY", "d": {"user": {"id": BOT.to_string()}}})
    }

    /// The guild as GUILD_CREATE gives it, with who is in which channel.
    fn guild(states: &[(u64, u64)]) -> Value {
        let states: Vec<Value> = states
            .iter()
            .map(|(user, channel)| {
                json!({
                    "user_id": user.to_string(), "channel_id": channel.to_string(),
                    "session_id": "s", "self_mute": false, "self_deaf": false,
                })
            })
            .collect();
        json!({"t": "GUILD_CREATE", "d": {"id": GUILD.to_string(), "voice_states": states}})
    }

    /// A member's voice state as VOICE_STATE_UPDATE gives it: in `channel`,
    /// or in none, muted or not.
    fn update(user: u64, channel: Option<u64>, mute: bool) -> Value {
        json!({"t": "VOICE_STATE_UPDATE", "d": {
            "guild_id": GUILD.to_string(),
            "user_id": user.to_string(),
            "channel_id": channel.map(|channel| channel.to_string()),
            "session_id": "s", "self_mute": mute, "self_deaf": false,
            "member": {"user": {"id": user.to_string(), "username": "ada", "global_name": "Ada"}},
        }})
    }

    /// `update` from the voice session `session`.
    fn in_session(mut update: Value, session: &str) -> Value {
        update["d"]["session_id"] = json!(session);
        update
    }

    /// Each dispatch in turn, as (which dispatch, user, presence) for every
    /// change one makes.
    fn changes(members: &mut Members, dispatches: &[Value]) -> Vec<(usize, u64, Presence)> {
        let mut changes = Vec::new();
        for (index, dispatch) in dispatches.iter().enumerate() {
            for change in members.observe(dispatch, 1_790_000_000_000) {
                assert_eq!(change.channel, VOICE);
                changes.push((index, change.user, change.presence));
            }
        }
        changes
    }

    /// The one change a dispatch makes.
    fn single(mut changes: Vec<PresenceChange>) -> PresenceChange {
        assert_eq!(changes.len(), 1, "{changes:?}");
        changes.remove(0)
    }

    /// Coming into the channel is a join, with the member's name, and going
    /// out of it, to no channel, a leave.
    #[test]
    fn a_join_and_a_leave_are_changes() {
        let mut members = members();
        members.observe(&ready(), 0);
        members.observe(&guild(&[]), 0);
        let joined = single(members.observe(&update(ADA, Some(VOICE), false), 7));
        assert_eq!(
            joined,
            PresenceChange {
                channel: VOICE,
                user: ADA,
                name: Some("Ada".to_owned()),
                presence: Presence::Joined,
                seen_ms: 7,
            }
        );
        assert_eq!(
            changes(&mut members, &[update(ADA, None, false)]),
            [(0, ADA, Presence::Left)]
        );
    }

    /// Moving into the channel from another is a join, and moving out of it
    /// to another a leave; moving between two other channels, or leaving
    /// one of them, is nothing.
    #[test]
    fn a_move_is_a_join_or_a_leave_of_this_channel_only() {
        let mut members = members();
        members.observe(&ready(), 0);
        members.observe(&guild(&[(ADA, ELSEWHERE)]), 0);
        assert_eq!(
            changes(
                &mut members,
                &[
                    update(ADA, Some(VOICE), false),
                    update(ADA, Some(ELSEWHERE), false),
                    update(ADA, Some(ELSEWHERE + 1), false),
                    update(ADA, None, false),
                ]
            ),
            [(0, ADA, Presence::Joined), (1, ADA, Presence::Left)]
        );
    }

    /// Muting, unmuting or anything else of a member who stays in the
    /// channel is not a change, nor anything of one who stays out of it.
    #[test]
    fn a_state_change_while_present_is_not_a_change() {
        let mut members = members();
        members.observe(&ready(), 0);
        members.observe(&guild(&[(ADA, VOICE), (GRACE, ELSEWHERE)]), 0);
        assert_eq!(
            changes(
                &mut members,
                &[
                    update(ADA, Some(VOICE), true),
                    update(ADA, Some(VOICE), false),
                    update(GRACE, Some(ELSEWHERE), true),
                ]
            ),
            NONE
        );
    }

    /// The bot's own comings and goings are never a change, nor is anything
    /// in another guild.
    #[test]
    fn the_bots_own_updates_and_other_guilds_are_not_changes() {
        let mut members = members();
        members.observe(&ready(), 0);
        members.observe(&guild(&[]), 0);
        let mut other_guild = update(ADA, Some(VOICE), false);
        other_guild["d"]["guild_id"] = json!((GUILD + 1).to_string());
        assert_eq!(
            changes(
                &mut members,
                &[
                    update(BOT, Some(VOICE), false),
                    update(BOT, None, false),
                    other_guild,
                ]
            ),
            NONE
        );
    }

    /// GUILD_CREATE is compared with what the process knew: at its start,
    /// nothing, so whoever it lists in the channel is seen coming in then
    /// (not the bot), each a notice of its own; after a new session, what
    /// the old one knew, so whoever came or went while there was none is
    /// seen then, and whoever stayed is no change. Before the GUILD_CREATE
    /// nothing is a change, since a mute cannot be told from a join. A
    /// resumed session keeps what it knew.
    #[test]
    fn a_guild_create_is_compared_with_what_was_known() {
        let mut members = members();
        members.observe(&ready(), 0);
        assert_eq!(
            changes(&mut members, &[update(ADA, Some(VOICE), false)]),
            NONE,
            "before GUILD_CREATE"
        );
        let now = 1_790_000_000_000;
        let seen: Vec<_> = members
            .observe(&guild(&[(ADA, VOICE), (GRACE, VOICE), (BOT, VOICE)]), now)
            .into_iter()
            .map(|change| (change.user, change.presence, change.seen_ms))
            .collect();
        assert_eq!(
            seen,
            [
                (ADA, Presence::Joined, now),
                (GRACE, Presence::Joined, now + 1)
            ],
            "at the process's start"
        );
        assert_eq!(
            changes(&mut members, &[update(ADA, Some(VOICE), true)]),
            NONE
        );
        // A new session knows nothing of the last one until its
        // GUILD_CREATE, which says Grace is gone and Eve is there; Ada, who
        // stayed, is no change, and her leaving afterwards is one.
        let eve = GRACE + 1;
        assert_eq!(
            changes(
                &mut members,
                &[
                    ready(),
                    update(GRACE, None, false),
                    guild(&[(ADA, VOICE), (eve, VOICE)]),
                    update(ADA, Some(VOICE), true),
                    update(ADA, None, false),
                ]
            ),
            [
                (2, eve, Presence::Joined),
                (2, GRACE, Presence::Left),
                (4, ADA, Presence::Left)
            ]
        );
        // Resumed: Discord replays what the session missed, which is a
        // change against what it knew.
        assert_eq!(
            changes(
                &mut members,
                &[
                    json!({"t": "RESUMED", "d": {}}),
                    update(ADA, Some(VOICE), false),
                ]
            ),
            [(1, ADA, Presence::Joined)]
        );
    }

    /// A member whose client comes back under a new voice session stays in
    /// the channel, and the end of their older session, reported after the
    /// new one, is not their leaving (nor, for the bot's own account, is it
    /// the voice connection's: `voice::VoiceJoin`). Their leaving with the
    /// session they are there with is. The older session ending before the
    /// new one begins is what Discord says happened: a leave and a join.
    #[test]
    fn the_end_of_an_older_voice_session_is_not_a_leave() {
        let mut members = members();
        members.observe(&ready(), 0);
        members.observe(&guild(&[(ADA, VOICE), (GRACE, VOICE)]), 0);
        assert_eq!(
            changes(
                &mut members,
                &[
                    in_session(update(ADA, Some(VOICE), false), "a2"),
                    in_session(update(ADA, None, false), "s"),
                    in_session(update(ADA, Some(VOICE), true), "a2"),
                    in_session(update(ADA, None, false), "a2"),
                    in_session(update(GRACE, None, false), "s"),
                    in_session(update(GRACE, Some(VOICE), false), "g2"),
                ]
            ),
            [
                (3, ADA, Presence::Left),
                (4, GRACE, Presence::Left),
                (5, GRACE, Presence::Joined),
            ]
        );
    }

    /// Changes handled in one burst, as Discord's replay of what a resumed
    /// session missed is, are each seen a millisecond after the one before
    /// at least, so each is a notice of its own, in the order they happened;
    /// so are changes seen while the clock went back.
    #[test]
    fn changes_in_one_burst_are_seen_in_the_order_they_happened() {
        let mut members = members();
        members.observe(&ready(), 0);
        members.observe(&guild(&[]), 0);
        let now = 1_790_000_000_000;
        let mut seen = Vec::new();
        for (dispatch, at) in [
            (update(ADA, Some(VOICE), false), now),
            (update(ADA, None, false), now),
            (update(ADA, Some(VOICE), false), now),
            (update(ADA, None, false), now - 5_000),
            (update(ADA, Some(VOICE), false), now + 60_000),
        ] {
            let change = single(members.observe(&dispatch, at));
            seen.push((change.seen_ms, change.presence));
        }
        assert_eq!(
            seen,
            [
                (now, Presence::Joined),
                (now + 1, Presence::Left),
                (now + 2, Presence::Joined),
                (now + 3, Presence::Left),
                (now + 60_000, Presence::Joined),
            ]
        );
    }
}
