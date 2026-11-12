//! Alarm operations over a throwaway pile, with a fake clock and a fake
//! rung executor: nothing here reaches Discord, the Mac, a light or a voice.

use std::fs::File;

use super::*;
use crate::alarm::guards::THE_CHECK;
use crate::alarm::ladder::Capabilities;
use crate::alarm::rungs::RungResult;
use crate::relations::{ProfileInput, Relations};
use crate::schemas::alarm::{RUNG_COLONY, RUNG_DISCORD_MENTION, VIA_CLI};
use crate::storage::initialize_signer;

/// 2026-11-11 in TAI nanoseconds, roughly; only differences matter.
pub(crate) const T0: i128 = 1_978_000_000 * SECOND;

pub(crate) struct FakeClock {
    pub now: i128,
}

impl Clock for FakeClock {
    fn now(&mut self) -> Result<i128> {
        Ok(self.now)
    }

    fn sleep_until(&mut self, at: i128) -> Result<()> {
        self.now = self.now.max(at);
        Ok(())
    }
}

/// Records every rung it is asked to run and delivers it; a Discord rung
/// gets a fresh snowflake.
#[derive(Default)]
pub(crate) struct FakeRunner {
    pub ran: Vec<(Id, String)>,
    pub next_snowflake: u64,
}

impl RungRunner for FakeRunner {
    fn host(&self) -> &str {
        "sky"
    }

    fn capabilities(&mut self, _subject: &Subject) -> Capabilities {
        Capabilities::default()
    }

    fn run(&mut self, subject: &Subject, due: Due) -> RungResult {
        self.ran.push((due.rung, subject.what.clone()));
        let mut result = RungResult::delivered();
        if due.rung == RUNG_DISCORD_MENTION {
            self.next_snowflake += 1;
            result.discord_message_id =
                Some((100_000_000_000_000_500 + self.next_snowflake).to_string());
        }
        result
    }
}

pub(crate) struct Fixture {
    pub _directory: tempfile::TempDir,
    pub storage: Storage,
    pub alarm: Alarm,
}

/// A pile with three zooids (cyan, astra, and a retired one) and jp.
pub(crate) fn fixture() -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let pile = directory.path().join("alarm.pile");
    let key = directory.path().join("alarm.key");
    File::create(&pile).unwrap();
    initialize_signer(&pile, Some(&key)).unwrap();
    let storage = Storage::new(pile, Some(key));
    let relations = Relations::with_storage(storage.clone());
    let person = |label: &str, zooid: bool| ProfileInput {
        label: label.to_owned(),
        affinities: if zooid {
            vec![ZOOID_AFFINITY.to_owned()]
        } else {
            Vec::new()
        },
        ..Default::default()
    };
    // Labels are not hex, so no label can also read as an id prefix.
    relations.add(person("cyan", true), None, &[]).unwrap();
    relations.add(person("astra", true), None, &[]).unwrap();
    relations.add(person("old-zooid", true), None, &[]).unwrap();
    relations.retire("old-zooid").unwrap();
    relations.add(person("jp", false), None, &[]).unwrap();
    Fixture {
        _directory: directory,
        alarm: Alarm::with_storage(storage.clone()),
        storage,
    }
}

pub(crate) fn request<'a>(raiser: &'a str, what: &'a str) -> RaiseRequest<'a> {
    RaiseRequest {
        raiser,
        for_person: "jp",
        class: "data-loss",
        host: "sky",
        what,
        decays: "the APFS snapshot cycles out at 04:00",
        evidence: "self.pile is 19 KB, was 6.1 GB at 01:00",
        incident: None,
        test: false,
        separate: false,
    }
}

#[test]
fn an_alarm_raise_runs_r0_and_records_each_rung_as_a_step() {
    let fixture = fixture();
    let mut clock = FakeClock { now: T0 };
    let mut runner = FakeRunner::default();
    let raised = fixture
        .alarm
        .raise(&request("cyan", "pile truncated"), &mut runner, &mut clock)
        .unwrap();
    let Raised::Alarm {
        alarm,
        subject,
        steps,
    } = raised
    else {
        panic!("a first raise starts a ladder: {raised:?}");
    };
    assert_eq!(subject.alarm, alarm);
    assert_eq!(subject.raiser_label, "cyan");
    assert_eq!(subject.class, "data-loss");
    let rungs: Vec<Id> = runner.ran.iter().map(|(rung, _)| *rung).collect();
    assert_eq!(rungs, vec![RUNG_COLONY, RUNG_DISCORD_MENTION]);
    assert_eq!(steps.len(), 2);
    assert!(steps.iter().all(|step| step.outcome == Outcome::Delivered));

    let shown = fixture.alarm.show(&format!("{alarm:x}")[..6]).unwrap();
    assert!(shown.open);
    assert_eq!(shown.subject.what, "pile truncated");
    assert_eq!(
        shown.subject.decays,
        "the APFS snapshot cycles out at 04:00"
    );
    assert_eq!(
        shown.subject.evidence,
        "self.pile is 19 KB, was 6.1 GB at 01:00"
    );
    assert_eq!(shown.subject.host, "sky");
    assert_eq!(shown.subject.raised_at, T0);
    // Both steps started at the same (fake) instant, so their order is
    // only their ids': compare them as a set.
    let mut rungs: Vec<&str> = shown.steps.iter().map(|step| step.rung.as_str()).collect();
    rungs.sort();
    assert_eq!(rungs, ["colony", "discord-mention"]);
    assert!(shown
        .steps
        .iter()
        .all(|step| step.delivered && !step.failed));
    let listed = fixture.alarm.list(true, &mut clock).unwrap();
    assert_eq!(listed.alarms.len(), 1);
    assert_eq!(listed.alarms[0].raisers, vec!["cyan".to_owned()]);
    assert_eq!(listed.alarms[0].steps, 2);
}

#[test]
fn an_alarm_raise_by_someone_who_is_not_a_live_zooid_is_refused() {
    let fixture = fixture();
    let mut clock = FakeClock { now: T0 };
    let mut runner = FakeRunner::default();
    for (raiser, says) in [
        ("jp", "not a zooid"),
        ("old-zooid", "not an active Relations person"),
        ("nobody", "not an active Relations person"),
        ("", "no raiser"),
    ] {
        let error = fixture
            .alarm
            .raise(&request(raiser, "pile truncated"), &mut runner, &mut clock)
            .unwrap_err();
        assert!(format!("{error:#}").contains(says), "{raiser}: {error:#}");
    }
    assert!(runner.ran.is_empty());
    assert!(fixture
        .alarm
        .list(false, &mut clock)
        .unwrap()
        .alarms
        .is_empty());
}

#[test]
fn an_alarm_raise_without_its_reason_is_refused_with_the_check_and_writes_nothing() {
    let fixture = fixture();
    let mut clock = FakeClock { now: T0 };
    let mut runner = FakeRunner::default();
    let mut bare = request("cyan", "pile truncated");
    bare.decays = "  ";
    let error = fixture
        .alarm
        .raise(&bare, &mut runner, &mut clock)
        .unwrap_err();
    assert!(format!("{error:#}").contains(THE_CHECK), "{error:#}");
    let mut unknown = request("cyan", "pile truncated");
    unknown.class = "unsure";
    let error = fixture
        .alarm
        .raise(&unknown, &mut runner, &mut clock)
        .unwrap_err();
    assert!(format!("{error:#}").contains(THE_CHECK), "{error:#}");
    assert!(runner.ran.is_empty());
    assert!(fixture
        .alarm
        .list(false, &mut clock)
        .unwrap()
        .alarms
        .is_empty());
}

#[test]
fn an_alarm_raise_the_guards_fold_becomes_a_note_and_runs_no_rung() {
    let fixture = fixture();
    let mut clock = FakeClock { now: T0 };
    let mut runner = FakeRunner::default();
    let Raised::Alarm { alarm, .. } = fixture
        .alarm
        .raise(&request("cyan", "pile truncated"), &mut runner, &mut clock)
        .unwrap()
    else {
        panic!("the first raise starts a ladder");
    };
    clock.now += 60 * SECOND;
    let again = fixture
        .alarm
        .raise(&request("astra", "pile truncated"), &mut runner, &mut clock)
        .unwrap();
    let Raised::Noted { on, .. } = again else {
        panic!("the same text within 30 min is a note: {again:?}");
    };
    assert_eq!(on, alarm);
    assert_eq!(runner.ran.len(), 2, "only the first raise ran R0");
    let shown = fixture.alarm.show(&format!("{alarm:x}")).unwrap();
    assert_eq!(shown.events.len(), 1);
    assert_eq!(shown.events[0].kind, "note");
    assert_eq!(shown.events[0].by, "astra");
    assert!(shown.events[0]
        .text
        .as_deref()
        .unwrap()
        .contains("raised again by astra"));
    assert_eq!(
        fixture.alarm.list(false, &mut clock).unwrap().alarms.len(),
        1
    );
}

#[test]
fn alarm_ack_resolve_and_note_close_annotate_and_count_false_alarms() {
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
    let id = format!("{alarm:x}");
    clock.now += 30 * SECOND;
    let noted = fixture
        .alarm
        .note(&id[..8], "cyan", "the copy is running", &mut clock)
        .unwrap();
    assert!(!noted.was_closed);
    let error = fixture
        .alarm
        .ack(&id, "cyan", VIA_RELAYED, None, &mut clock)
        .unwrap_err();
    assert!(format!("{error:#}").contains("evidence"), "{error:#}");
    clock.now += 30 * SECOND;
    let acked = fixture
        .alarm
        .ack(&id, "jp", VIA_CLI, None, &mut clock)
        .unwrap();
    assert_eq!(acked.alarm, alarm);
    assert!(!acked.was_closed);
    let listed = fixture.alarm.list(false, &mut clock).unwrap();
    assert!(!listed.alarms[0].open);
    assert_eq!(listed.alarms[0].closed_at, Some(T0 + 60 * SECOND));
    assert!(fixture
        .alarm
        .list(true, &mut clock)
        .unwrap()
        .alarms
        .is_empty());
    let shown = fixture.alarm.show(&id).unwrap();
    assert!(!shown.open);
    assert_eq!(
        shown
            .events
            .iter()
            .map(|event| (event.kind, event.by.as_str()))
            .collect::<Vec<_>>(),
        [("note", "cyan"), ("ack", "jp")]
    );
    assert_eq!(shown.events[1].via.as_deref(), Some("cli"));

    // A second alarm, by another zooid, resolved as false.
    clock.now += 2 * 60 * SECOND;
    let Raised::Alarm { alarm: second, .. } = fixture
        .alarm
        .raise(&request("astra", "host hung"), &mut runner, &mut clock)
        .unwrap()
    else {
        panic!("a closed alarm does not hold the ladder");
    };
    let resolved = fixture
        .alarm
        .resolve(
            &format!("{second:x}"),
            "astra",
            "it was a slow disk",
            true,
            &mut clock,
        )
        .unwrap();
    assert!(!resolved.was_closed);
    let listed = fixture.alarm.list(false, &mut clock).unwrap();
    assert_eq!(listed.false_alarms, vec![("astra".to_owned(), 1)]);
    assert!(listed.alarms.iter().all(|alarm| !alarm.open));
    // Acking a closed alarm is recorded and says so.
    let late = fixture
        .alarm
        .ack(&id, "jp", VIA_CLI, None, &mut clock)
        .unwrap();
    assert!(late.was_closed);
}
