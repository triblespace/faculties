//! Ack detection over synthetic Discord fragments shaped like what intake,
//! a send receipt, the hearing and presence store.

use triblespace::core::repo::SnapshotSource;

use super::*;
use crate::alarm::operations::tests::{fixture, request, FakeClock, FakeRunner, T0};
use crate::alarm::operations::Raised;
use crate::alarm::{point_at, raise_fragment, step_fragment, Outcome, Raise, Step, MINUTE};
use crate::discord::{
    bot_account_fragment, channel_fragment, message_anchor_fragment, presence_fragment,
    user_fragment, utterance_fragment, Presence,
};
use crate::schemas::alarm::{CLASS_DATA_LOSS, RUNG_DISCORD_MENTION};
use crate::schemas::discord::DEFAULT_SCOPE_ID as DISCORD_SCOPE_ID;

const JP: &str = "100000000000000777";
const SOMEONE: &str = "100000000000000888";
const BOT: &str = "100000000000000999";
const CHANNEL: &str = "100000000000000101";
const VOICE: &str = "100000000000000105";
/// The snowflake the alarm's mention got.
const POSTED: &str = "100000000000000501";

/// One stored Discord message: its anchor, author and channel anchors, and
/// the observation linking them, as intake builds it.
pub(crate) fn message(
    snowflake: &str,
    author: &str,
    content: &str,
    at: i128,
    reply_to: Option<&str>,
) -> Fragment {
    let mut fragment = channel_fragment(CHANNEL).unwrap();
    let channel = fragment.root().unwrap();
    let anchor = message_anchor_fragment(snowflake).unwrap();
    let anchor_id = anchor.root().unwrap();
    fragment += anchor;
    let user = user_fragment(author).unwrap();
    let user_id = user.root().unwrap();
    fragment += user;
    let reply = reply_to.map(|target| {
        let anchor = message_anchor_fragment(target).unwrap();
        let id = anchor.root().unwrap();
        fragment += anchor;
        id
    });
    fragment += entity! { _ @
        metadata::tag: archive::kind_message,
        discord::message: anchor_id,
        discord::channel: channel,
        archive::author: user_id,
        archive::content: content.to_owned(),
        metadata::created_at: point_at(at).unwrap(),
        archive::reply_to?: reply,
    };
    fragment
}

fn anchor(snowflake: &str) -> Id {
    message_anchor_fragment(snowflake).unwrap().root().unwrap()
}

/// An alarm raised at T0 whose mention got [`POSTED`].
fn alarm() -> (TribleSet, Id) {
    let (raised, alarm) = raise_fragment(&Raise {
        raiser: Id::new([0x11; 16]).unwrap(),
        for_person: Id::new([0x10; 16]).unwrap(),
        class: CLASS_DATA_LOSS,
        host: "sky".to_owned(),
        raised_at: T0,
        what: "pile truncated".to_owned(),
        decays: "the snapshot cycles out at 04:00".to_owned(),
        evidence: "19 KB".to_owned(),
        incident: None,
        test: false,
    })
    .unwrap();
    let (stepped, _) = step_fragment(&Step {
        alarm,
        rung: RUNG_DISCORD_MENTION,
        host: "sky".to_owned(),
        started: T0,
        finished: T0 + 1,
        outcome: Outcome::Delivered,
        discord_message: None,
        discord_message_id: Some(POSTED.to_owned()),
        say_request: None,
    })
    .unwrap();
    let mut facts = raised.into_facts();
    facts += stepped.into_facts();
    (facts, alarm)
}

/// Detect over these Discord fragments, reading their texts from them.
fn detect_in(fragments: Vec<Fragment>, jp: &str) -> Option<Detected> {
    let (facts, alarm) = alarm();
    let mut discord = Fragment::empty();
    for fragment in fragments {
        discord += fragment;
    }
    let (discord, mut blobs) = discord.into_facts_and_blobs();
    let reader = blobs.snapshot().unwrap();
    detect(&facts, &discord, &reader, alarm, jp)
}

#[test]
fn a_jp_reply_to_the_alarm_message_acks_it() {
    let detected = detect_in(
        vec![
            message(POSTED, BOT, "<@777> ALARM ...", T0, None),
            message(
                "100000000000000600",
                JP,
                "what happened?",
                T0 + 4 * MINUTE,
                Some(POSTED),
            ),
        ],
        JP,
    )
    .expect("a reply to the alarm's message acks it, whatever it says");
    assert_eq!(detected.via, VIA_DISCORD_REPLY);
    assert_eq!(detected.message, anchor("100000000000000600"));
    assert_eq!(detected.at, T0 + 4 * MINUTE);
    // A reply to some other message is not about this alarm.
    assert_eq!(
        detect_in(
            vec![message(
                "100000000000000601",
                JP,
                "what happened?",
                T0 + MINUTE,
                Some("100000000000000502")
            )],
            JP,
        ),
        None
    );
}

#[test]
fn a_jp_seen_after_the_raise_acks_and_the_same_line_before_it_does_not() {
    let detected = detect_in(
        vec![message(
            "100000000000000602",
            JP,
            "  Seen. ",
            T0 + 2 * MINUTE,
            None,
        )],
        JP,
    )
    .expect("a whitelisted line after the raise acks");
    assert_eq!(detected.via, VIA_DISCORD_LINE);
    assert_eq!(detected.message, anchor("100000000000000602"));
    assert_eq!(
        detect_in(
            vec![message("100000000000000603", JP, "seen", T0 - MINUTE, None)],
            JP
        ),
        None,
        "a line from before the raise is not about it"
    );
    assert_eq!(
        detect_in(
            vec![message("100000000000000604", JP, "seen", T0, None)],
            JP
        ),
        None,
        "nor is one at the very instant of the raise"
    );
    // The alarm's own short id acks too.
    let (_, alarm) = alarm();
    let id8 = format!("{alarm:x}")[..8].to_owned();
    assert!(detect_in(
        vec![message("100000000000000605", JP, &id8, T0 + MINUTE, None)],
        JP
    )
    .is_some());
}

#[test]
fn a_seen_from_someone_else_or_from_the_bot_does_not_ack() {
    // JP is known to the pile (an earlier, unrelated line of his), so his
    // user anchor exists and only the author constraint keeps the other
    // person's "seen" from counting.
    assert_eq!(
        detect_in(
            vec![
                message(
                    "100000000000000612",
                    JP,
                    "good night",
                    T0 - 60 * MINUTE,
                    None
                ),
                message("100000000000000606", SOMEONE, "seen", T0 + MINUTE, None),
            ],
            JP
        ),
        None
    );
    let bot_line = || {
        let mut fragment = message("100000000000000607", BOT, "seen", T0 + MINUTE, None);
        fragment += bot_account_fragment(BOT).unwrap();
        fragment
    };
    assert_eq!(detect_in(vec![bot_line()], JP), None);
    // Even configured as JP, the pile's own bot account never counts as him.
    assert_eq!(detect_in(vec![bot_line()], BOT), None);
    // And with nobody configured, nothing acks.
    assert_eq!(
        detect_in(
            vec![message("100000000000000608", JP, "seen", T0 + MINUTE, None)],
            " "
        ),
        None
    );
}

#[test]
fn a_jp_utterance_seen_it_acks_via_voice() {
    let spoken = utterance_fragment(
        VOICE,
        JP,
        point_at(T0 + 5 * MINUTE).unwrap(),
        "Seen it.",
        None,
        None,
    )
    .unwrap();
    let detected = detect_in(vec![spoken], JP).expect("a spoken seen acks");
    assert_eq!(detected.via, VIA_VOICE);
    assert_eq!(detected.at, T0 + 5 * MINUTE);
}

#[test]
fn a_jp_line_unrelated_to_the_alarm_does_not_ack() {
    for line in [
        "seen the new build yet?",
        "ok so about the lamps",
        "I have not seen it",
        "",
    ] {
        assert_eq!(
            detect_in(
                vec![message("100000000000000609", JP, line, T0 + MINUTE, None)],
                JP
            ),
            None,
            "{line:?}"
        );
    }
    assert!(
        acks("OKAY!", "abcdef01") && acks("on   it", "abcdef01") && acks("ABCDEF01", "abcdef01")
    );
}

#[test]
fn jp_is_in_voice_between_his_join_and_his_leave() {
    let notice = |presence, at| {
        presence_fragment(VOICE, JP, Some("JP"), presence, point_at(at).unwrap()).unwrap()
    };
    let facts = |fragments: Vec<Fragment>| {
        let mut all = Fragment::empty();
        for fragment in fragments {
            all += fragment;
        }
        all.into_facts()
    };
    assert!(!jp_in_voice(&facts(vec![]), JP));
    assert!(jp_in_voice(&facts(vec![notice(Presence::Joined, T0)]), JP));
    assert!(!jp_in_voice(
        &facts(vec![
            notice(Presence::Joined, T0),
            notice(Presence::Left, T0 + MINUTE)
        ]),
        JP
    ));
    assert!(jp_in_voice(
        &facts(vec![
            notice(Presence::Joined, T0),
            notice(Presence::Left, T0 + MINUTE),
            notice(Presence::Joined, T0 + 2 * MINUTE)
        ]),
        JP
    ));
    assert!(!jp_in_voice(
        &facts(vec![notice(Presence::Joined, T0)]),
        SOMEONE
    ));
}

#[test]
fn a_detected_alarm_ack_is_listed_until_recorded_then_closes_the_alarm() {
    let fixture = fixture();
    let mut clock = FakeClock { now: T0 };
    let mut runner = FakeRunner::default();
    let Raised::Alarm { alarm, .. } = fixture
        .alarm
        .raise(&request("cyan", "pile truncated"), &mut runner, &mut clock)
        .unwrap()
    else {
        panic!("a first raise starts a ladder");
    };
    // FakeRunner gives the first mention this snowflake.
    let posted = "100000000000000501";
    let reply = message(
        "100000000000000610",
        JP,
        "on my way",
        T0 + 4 * MINUTE,
        Some(posted),
    );
    fixture
        .storage
        .with_store(|pile, signer, runtime| {
            crate::alarm::operations::commit_to(
                &fixture.storage,
                pile,
                signer,
                runtime,
                DISCORD_SCOPE_ID,
                "Discord",
                reply,
            )
        })
        .unwrap();
    clock.now = T0 + 5 * MINUTE;
    let listed = fixture.alarm.list(false, Some(JP), &mut clock).unwrap();
    assert!(listed.alarms[0].open);
    assert_eq!(listed.alarms[0].pending_ack, Some("discord-reply"));
    let detected = fixture.alarm.detect_ack(alarm, JP).unwrap().unwrap();
    assert_eq!(detected.via, VIA_DISCORD_REPLY);
    assert!(fixture
        .alarm
        .record_detected(alarm, detected)
        .unwrap()
        .is_some());
    let listed = fixture.alarm.list(false, Some(JP), &mut clock).unwrap();
    assert!(!listed.alarms[0].open);
    assert_eq!(listed.alarms[0].pending_ack, None);
    let shown = fixture.alarm.show(&format!("{alarm:x}")).unwrap();
    assert_eq!(shown.events.len(), 1);
    assert_eq!(
        (shown.events[0].kind, shown.events[0].by.as_str()),
        ("ack", "jp")
    );
    assert_eq!(shown.events[0].via.as_deref(), Some("discord-reply"));
    assert_eq!(shown.events[0].at, T0 + 4 * MINUTE);
    // Recording it again writes nothing: the alarm is closed.
    assert!(fixture
        .alarm
        .record_detected(alarm, detected)
        .unwrap()
        .is_none());
}
