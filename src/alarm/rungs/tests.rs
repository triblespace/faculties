//! The executor over a recording fake transport: no Discord call, no ssh,
//! no sound, no light and no voice leaves the process.

use super::*;
use crate::alarm::ladder::{next_action, Next};
use crate::alarm::operations::tests::{fixture, request, FakeClock, T0};
use crate::alarm::operations::Raised;
use crate::alarm::{steps, Outcome, MINUTE};

const JP: &str = "100000000000000777";

/// Records every outside effect as one line, and fails any whose line
/// contains one of `fail`.
#[derive(Default)]
pub(crate) struct FakeTransport {
    pub calls: Vec<String>,
    pub fail: Vec<&'static str>,
    pub volume: String,
    pub next_snowflake: u64,
}

impl FakeTransport {
    fn record(&mut self, call: String) -> Result<()> {
        let refused = self
            .fail
            .iter()
            .find(|pattern| call.contains(**pattern))
            .copied();
        self.calls.push(call);
        match refused {
            Some(pattern) => bail!("fake transport refused {pattern:?}"),
            None => Ok(()),
        }
    }
}

impl Transport for FakeTransport {
    fn message(&mut self, from: Id, to: &str, text: &str) -> Result<()> {
        self.record(format!("message from {from:x} to {to}: {text}"))
    }

    fn discord_send(&mut self, channel: &str, text: &str) -> Result<String> {
        self.record(format!("discord {channel}: {text}"))?;
        self.next_snowflake += 1;
        Ok((100_000_000_000_000_900 + self.next_snowflake).to_string())
    }

    fn discord_ready(&mut self) -> Result<()> {
        self.record("discord_ready".to_owned())
    }

    fn say(&mut self, guild: Option<&str>, channel: &str, text: &str) -> Result<Id> {
        self.record(format!("say {guild:?} {channel}: {text}"))?;
        Ok(Id::new([0x5a; 16]).unwrap())
    }

    fn run(&mut self, program: &str, args: &[String]) -> Result<String> {
        let call = format!("run {program} {}", args.join(" "));
        let volume = call.contains("output volume of");
        self.record(call)?;
        Ok(if volume {
            self.volume.clone()
        } else {
            String::new()
        })
    }

    fn hue_alert(&mut self, target: &str) -> Result<()> {
        self.record(format!("hue {target}"))
    }
}

pub(crate) fn settings() -> Settings {
    Settings {
        jp_discord_user: Some(JP.to_owned()),
        discord_channel: Some("100000000000000101".to_owned()),
        test_channel: Some("100000000000000104".to_owned()),
        voice_channel: Some("100000000000000105".to_owned()),
        voice_guild: None,
        colony_group: Some("the-colony".to_owned()),
        mac_ssh: Some("mac".to_owned()),
        hue_target: Some("Kim's lights".to_owned()),
    }
}

fn subject(test: bool) -> Subject {
    Subject {
        alarm: Id::new([0xab; 16]).unwrap(),
        raiser: Id::new([0x11; 16]).unwrap(),
        raiser_label: "cc".to_owned(),
        class: "data-loss".to_owned(),
        host: "sky".to_owned(),
        what: "self.pile is 19 KB".to_owned(),
        decays: "the snapshot cycles out at 04:00".to_owned(),
        evidence: "ls -l".to_owned(),
        test,
        raised_at: T0,
    }
}

fn executor(transport: FakeTransport, settings: Settings) -> Executor<FakeTransport> {
    Executor::new(transport, settings, "sky")
}

#[test]
fn a_failing_alarm_transport_records_a_failed_step_and_the_next_rung_still_runs() {
    let fixture = fixture();
    let mut clock = FakeClock { now: T0 };
    let mut runner = executor(
        FakeTransport {
            fail: vec!["message from"],
            ..Default::default()
        },
        settings(),
    );
    let Raised::Alarm {
        alarm, steps: ran, ..
    } = fixture
        .alarm
        .raise(&request("cyan", "pile truncated"), &mut runner, &mut clock)
        .unwrap()
    else {
        panic!("a first raise starts a ladder");
    };
    assert!(matches!(&ran[0].outcome, Outcome::Failed(error) if error.contains("refused")));
    assert_eq!(ran[1].outcome, Outcome::Delivered);
    assert!(runner
        .transport
        .calls
        .iter()
        .any(|call| call.starts_with("discord 100000000000000101: <@")));
    let shown = fixture.alarm.show(&format!("{alarm:x}")).unwrap();
    let step = |rung: &str| shown.steps.iter().find(|step| step.rung == rung).unwrap();
    assert_eq!(shown.steps.len(), 2);
    assert!(step("colony").failed && !step("colony").delivered);
    assert!(step("colony").error[0].contains("fake transport refused"));
    assert!(step("discord-mention").delivered && !step("discord-mention").failed);
    // The failed colony step covers its occurrence: the ladder goes on to R1.
    let facts = fixture
        .storage
        .with_store(|pile, signer, runtime| {
            crate::alarm::operations::observe(&fixture.storage, pile, signer, runtime, true, false)
                .map(|view| view.alarm)
        })
        .unwrap();
    let rows = steps(&facts, alarm);
    assert_eq!(
        next_action(T0, T0 + MINUTE, &rows, true, false, &Default::default()),
        Next::Wait(T0 + 3 * MINUTE)
    );
}

#[test]
fn the_alarm_mention_pings_jp_by_id_and_a_test_pings_nobody() {
    let mut runner = executor(FakeTransport::default(), settings());
    let real = subject(false);
    let first = runner.run(
        &real,
        Due {
            rung: RUNG_DISCORD_MENTION,
            at: 0,
        },
    );
    assert_eq!(first.outcome, Outcome::Delivered);
    assert!(first.discord_message_id.is_some());
    let again = runner.run(
        &real,
        Due {
            rung: RUNG_DISCORD_MENTION,
            at: 3 * MINUTE,
        },
    );
    assert_eq!(again.outcome, Outcome::Delivered);
    let calls = runner.transport.calls.clone();
    assert!(calls[0].starts_with(&format!(
        "discord 100000000000000101: <@{JP}> ALARM (data-loss)"
    )));
    assert!(calls[0].contains("Decays: the snapshot cycles out at 04:00"));
    assert!(calls[0].contains("[alarm abababab]"));
    assert!(calls[1].contains(&format!("<@{JP}>")) && calls[1].contains("after 3 min"));
    runner.run(
        &real,
        Due {
            rung: RUNG_COLONY,
            at: 0,
        },
    );
    assert!(runner.transport.calls[2].starts_with(
        "message from 11111111111111111111111111111111 to the-colony: ALARM abababab"
    ));

    let mut runner = executor(FakeTransport::default(), settings());
    let test = subject(true);
    runner.run(
        &test,
        Due {
            rung: RUNG_DISCORD_MENTION,
            at: 0,
        },
    );
    runner.run(
        &test,
        Due {
            rung: RUNG_COLONY,
            at: 0,
        },
    );
    let calls = &runner.transport.calls;
    assert!(
        calls[0].starts_with("discord 100000000000000104: TEST alarm"),
        "{calls:?}"
    );
    assert!(calls.iter().all(|call| !call.contains("<@")), "{calls:?}");
    assert!(
        calls[1].starts_with("message from 11111111111111111111111111111111 to 11111111111111111111111111111111: TEST ALARM"),
        "a test's colony message goes to the raiser alone: {calls:?}"
    );
}

#[test]
fn the_alarm_sound_restores_the_volume_even_when_afplay_fails() {
    let mut runner = executor(
        FakeTransport {
            fail: vec!["afplay"],
            volume: "35\n".to_owned(),
            ..Default::default()
        },
        settings(),
    );
    let result = runner.run(
        &subject(false),
        Due {
            rung: RUNG_MAC_SOUND,
            at: 6 * MINUTE,
        },
    );
    assert!(matches!(&result.outcome, Outcome::Failed(error) if error.contains("play the sound")));
    let calls = &runner.transport.calls;
    assert!(
        calls
            .iter()
            .all(|call| call.starts_with("run ssh -o BatchMode=yes -o ConnectTimeout=10 mac -- ")),
        "{calls:?}"
    );
    assert!(
        calls[0].contains("'output volume of (get volume settings)'"),
        "{calls:?}"
    );
    assert!(
        calls[1].contains("'set volume output volume 80'"),
        "{calls:?}"
    );
    assert!(
        calls[2].contains("'afplay' '/System/Library/Sounds/Sosumi.aiff'"),
        "{calls:?}"
    );
    assert!(
        calls
            .last()
            .unwrap()
            .contains("'set volume output volume 35'"),
        "{calls:?}"
    );

    let mut runner = executor(
        FakeTransport {
            volume: "35".to_owned(),
            ..Default::default()
        },
        settings(),
    );
    let result = runner.run(
        &subject(false),
        Due {
            rung: RUNG_MAC_SOUND,
            at: 6 * MINUTE,
        },
    );
    assert_eq!(result.outcome, Outcome::Delivered);
    let calls = &runner.transport.calls;
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.contains("'afplay'"))
            .count(),
        ALARM_SOUND_REPEATS
    );
    assert!(calls
        .last()
        .unwrap()
        .contains("'set volume output volume 35'"));
}

#[test]
fn an_alarm_rung_this_host_cannot_reach_is_failed_as_unreachable_without_trying() {
    let mut partial = settings();
    partial.hue_target = None;
    partial.voice_channel = None;
    partial.mac_ssh = None;
    let mut runner = executor(
        FakeTransport {
            fail: vec!["discord_ready"],
            ..Default::default()
        },
        partial,
    )
    .probing_the_mac();
    let subject = subject(false);
    let capabilities = runner.capabilities(&subject);
    for rung in [
        RUNG_HUE_ALERT,
        RUNG_DISCORD_VOICE,
        RUNG_MAC_NOTIFY,
        RUNG_MAC_SOUND,
        RUNG_REACHY_SHOUT,
        RUNG_DISCORD_MENTION,
        RUNG_FINAL_DISCORD,
    ] {
        assert!(capabilities.unreachable.contains(&rung), "{rung:x}");
    }
    assert!(!capabilities.unreachable.contains(&RUNG_COLONY));
    let before = runner.transport.calls.len();
    let hue = runner.run(
        &subject,
        Due {
            rung: RUNG_HUE_ALERT,
            at: 10 * MINUTE,
        },
    );
    assert_eq!(
        hue.outcome,
        Outcome::Failed("unreachable from sky: ALARM_HUE_TARGET is not configured".to_owned())
    );
    let mention = runner.run(
        &subject,
        Due {
            rung: RUNG_DISCORD_MENTION,
            at: 0,
        },
    );
    assert!(matches!(&mention.outcome, Outcome::Failed(error)
        if error.starts_with("unreachable from sky: the Discord credential")));
    assert!(
        matches!(&runner.run(&subject, Due { rung: RUNG_MAC_SOUND, at: 6 * MINUTE }).outcome,
        Outcome::Failed(error) if error.contains("ALARM_MAC_SSH"))
    );
    assert_eq!(
        runner.transport.calls.len(),
        before,
        "an unreachable rung is never tried"
    );

    // A configured Mac that does not answer the probe is unreachable too.
    let mut runner = executor(
        FakeTransport {
            fail: vec!["'true'"],
            ..Default::default()
        },
        settings(),
    )
    .probing_the_mac();
    let capabilities = runner.capabilities(&subject);
    assert!(capabilities.unreachable.contains(&RUNG_MAC_SOUND));
    assert!(!capabilities.unreachable.contains(&RUNG_HUE_ALERT));
}

#[test]
fn alarm_words_are_quoted_for_ssh_and_applescript() {
    assert_eq!(shell_quote("it's"), r"'it'\''s'");
    assert_eq!(shell_quote("a b"), "'a b'");
    assert_eq!(
        applescript_string(r#"say "hi" \ now"#),
        r#""say \"hi\" \\ now""#
    );
    let mut runner = executor(FakeTransport::default(), settings());
    let mut quoted = subject(false);
    quoted.what = "JP's \"pile\" is gone".to_owned();
    runner.run(
        &quoted,
        Due {
            rung: RUNG_MAC_NOTIFY,
            at: 3 * MINUTE,
        },
    );
    let call = &runner.transport.calls[0];
    assert!(call.contains(r#"'display notification "JP'\''s \"pile\" is gone [alarm abababab]" with title "ALARM (data-loss)"'"#), "{call}");
    runner.run(
        &quoted,
        Due {
            rung: RUNG_HUE_ALERT,
            at: 10 * MINUTE,
        },
    );
    assert_eq!(runner.transport.calls[1], "hue Kim's lights");
}
