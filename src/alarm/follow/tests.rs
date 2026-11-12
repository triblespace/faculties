//! The follower on a fake clock with a fake transport and a fake spawner:
//! no unit is started, no Discord call is made, nothing sounds.

use std::cell::RefCell;

use super::*;
use crate::alarm::ack::tests::message;
use crate::alarm::operations::tests::{fixture, request, FakeClock, T0};
use crate::alarm::operations::{commit_to, Raised};
use crate::alarm::rungs::tests::{settings, FakeTransport};
use crate::alarm::rungs::Executor;
use crate::alarm::MINUTE;
use crate::schemas::alarm::VIA_DISCORD_REPLY;
use crate::schemas::discord::DEFAULT_SCOPE_ID as DISCORD_SCOPE_ID;

const JP: &str = "100000000000000777";

#[derive(Default)]
struct FakeSpawner {
    spawned: Vec<FollowerCommand>,
}

impl Spawner for FakeSpawner {
    fn spawn(&mut self, command: &FollowerCommand) -> Result<()> {
        self.spawned.push(command.clone());
        Ok(())
    }
}

fn command_for(pile: PathBuf) -> impl Fn(Id) -> Result<FollowerCommand> {
    move |alarm| {
        follower_command(
            Path::new("/opt/cohort/alarm"),
            &pile,
            &pile.with_extension("key"),
            alarm,
            Vec::new(),
        )
    }
}

#[test]
fn an_alarm_follower_runs_r1_records_jp_reply_as_the_ack_and_ends() {
    let fixture = fixture();
    let mut clock = FakeClock { now: T0 };
    let mut runner = Executor::new(FakeTransport::default(), settings(), "sky");
    let Raised::Alarm { alarm, .. } = fixture
        .alarm
        .raise(&request("cyan", "pile truncated"), &mut runner, &mut clock)
        .unwrap()
    else {
        panic!("a first raise starts a ladder");
    };
    // The raise starts a follower; here it is only recorded.
    let pile = fixture.storage.path().to_owned();
    let mut spawner = FakeSpawner::default();
    spawner
        .spawn(&command_for(pile.clone())(alarm).unwrap())
        .unwrap();
    assert_eq!(
        spawner.spawned[0].unit,
        format!("alarm-{}", &format!("{alarm:x}")[..8])
    );

    // JP replies to the first mention once R1 has re-mentioned him.
    let replied = RefCell::new(false);
    let mut pull = || -> Result<()> {
        let shown = fixture.alarm.show(&format!("{alarm:x}"))?;
        let mentions = shown
            .steps
            .iter()
            .filter(|step| step.rung == "discord-mention")
            .count();
        if mentions >= 2 && !*replied.borrow() {
            // The fake transport gave the first mention this snowflake.
            let reply = message(
                "100000000000000611",
                JP,
                "looking",
                T0 + 3 * MINUTE + 30 * SECOND,
                Some("100000000000000901"),
            );
            fixture.storage.with_store(|pile, signer, runtime| {
                commit_to(
                    &fixture.storage,
                    pile,
                    signer,
                    runtime,
                    DISCORD_SCOPE_ID,
                    "Discord",
                    reply,
                )
            })?;
            *replied.borrow_mut() = true;
        }
        Ok(())
    };
    let ended = follow(
        &fixture.alarm,
        alarm,
        &mut runner,
        &mut clock,
        Some(JP),
        &mut pull,
    )
    .unwrap();
    assert_eq!(
        ended,
        Ended::Acked {
            via: VIA_DISCORD_REPLY
        }
    );
    assert!(*replied.borrow());
    assert!(
        clock.now >= T0 + 3 * MINUTE && clock.now < T0 + 6 * MINUTE,
        "acked before R2"
    );

    let shown = fixture.alarm.show(&format!("{alarm:x}")).unwrap();
    assert!(!shown.open);
    let mut rungs: Vec<&str> = shown.steps.iter().map(|step| step.rung.as_str()).collect();
    rungs.sort();
    // R0, then R1 without the voice rung: JP is not in the voice channel.
    assert_eq!(
        rungs,
        ["colony", "discord-mention", "discord-mention", "mac-notify"]
    );
    assert_eq!(shown.events.len(), 1);
    assert_eq!(
        (shown.events[0].kind, shown.events[0].by.as_str()),
        ("ack", "jp")
    );
    assert_eq!(shown.events[0].via.as_deref(), Some("discord-reply"));
    let calls = &runner.transport.calls;
    assert!(
        calls.iter().any(|call| call.contains("after 3 min")),
        "{calls:?}"
    );
    assert!(
        calls.iter().all(|call| !call.contains("afplay")),
        "nothing sounded before the ack"
    );

    // A follower started on a closed alarm ends at once and runs nothing.
    let before = runner.transport.calls.len();
    let again = follow(
        &fixture.alarm,
        alarm,
        &mut runner,
        &mut clock,
        Some(JP),
        &mut || Ok(()),
    )
    .unwrap();
    assert_eq!(again, Ended::Closed);
    assert_eq!(runner.transport.calls.len(), before);
}

#[test]
fn a_test_alarm_follower_ends_after_r0() {
    let fixture = fixture();
    let mut clock = FakeClock { now: T0 };
    let mut runner = Executor::new(FakeTransport::default(), settings(), "sky");
    let mut test = request("cyan", "drill");
    test.test = true;
    let Raised::Alarm { alarm, .. } = fixture.alarm.raise(&test, &mut runner, &mut clock).unwrap()
    else {
        panic!("a test raise runs R0");
    };
    let ended = follow(
        &fixture.alarm,
        alarm,
        &mut runner,
        &mut clock,
        Some(JP),
        &mut || Ok(()),
    )
    .unwrap();
    assert_eq!(ended, Ended::Finished);
    assert_eq!(clock.now, T0);
}

#[test]
fn a_second_alarm_follower_is_refused_by_the_lock() {
    let directory = tempfile::tempdir().unwrap();
    let alarm = Id::new([0xab; 16]).unwrap();
    let first = Lock::acquire(directory.path(), alarm).unwrap();
    assert_eq!(
        Lock::holder(directory.path(), alarm),
        Some(std::process::id())
    );
    let error = Lock::acquire(directory.path(), alarm).unwrap_err();
    assert!(format!("{error:#}").contains("already runs"), "{error:#}");
    // Another alarm has its own lock.
    let other = Lock::acquire(directory.path(), Id::new([0xcd; 16]).unwrap()).unwrap();
    drop(other);
    drop(first);
    assert!(!Lock::path(directory.path(), alarm).exists());
    // A lock left by a process that is gone is taken over.
    std::fs::write(Lock::path(directory.path(), alarm), "4000000000\n").unwrap();
    assert_eq!(Lock::holder(directory.path(), alarm), None);
    let taken = Lock::acquire(directory.path(), alarm).unwrap();
    assert_eq!(
        Lock::holder(directory.path(), alarm),
        Some(std::process::id())
    );
    drop(taken);
}

#[test]
fn alarm_follow_resume_adopts_only_stale_open_alarms() {
    let fixture = fixture();
    let mut runner = Executor::new(FakeTransport::default(), settings(), "sky");
    let raise = |clock: &mut FakeClock,
                 runner: &mut Executor<FakeTransport>,
                 raiser,
                 what,
                 class,
                 separate,
                 test| {
        let mut asked = request(raiser, what);
        asked.class = class;
        asked.separate = separate;
        asked.test = test;
        match fixture.alarm.raise(&asked, runner, clock).unwrap() {
            Raised::Alarm { alarm, .. } => alarm,
            noted => panic!("{what}: {noted:?}"),
        }
    };
    // Closed long ago.
    let mut clock = FakeClock {
        now: T0 - 120 * MINUTE,
    };
    let closed = raise(
        &mut clock,
        &mut runner,
        "cyan",
        "old incident",
        "security",
        false,
        false,
    );
    clock.now = T0 - 110 * MINUTE;
    fixture
        .alarm
        .resolve(&format!("{closed:x}"), "cyan", "fixed", false, &mut clock)
        .unwrap();
    // A test, open and stale: tests are never adopted.
    clock.now = T0 - 60 * MINUTE;
    let test = raise(
        &mut clock,
        &mut runner,
        "astra",
        "a drill",
        "data-loss",
        false,
        true,
    );
    // Open and stale: its last step was at T0.
    clock.now = T0;
    let stale = raise(
        &mut clock,
        &mut runner,
        "cyan",
        "pile truncated",
        "data-loss",
        false,
        false,
    );
    // Open and fresh: raised four minutes before the resume.
    clock.now = T0 + 20 * MINUTE;
    let fresh = raise(
        &mut clock,
        &mut runner,
        "astra",
        "sky does not answer",
        "hung-host",
        true,
        false,
    );
    clock.now = T0 + 24 * MINUTE;

    let locks = tempfile::tempdir().unwrap();
    let pile = fixture.storage.path().to_owned();
    let mut spawner = FakeSpawner::default();
    let adopted = resume(
        &fixture.alarm,
        &mut clock,
        locks.path(),
        &mut spawner,
        &command_for(pile.clone()),
    )
    .unwrap();
    assert_eq!(adopted, vec![stale]);
    assert!(![closed, test, fresh]
        .iter()
        .any(|alarm| adopted.contains(alarm)));
    let command = &spawner.spawned[0];
    assert_eq!(
        command.unit,
        format!("alarm-{}", &format!("{stale:x}")[..8])
    );
    assert_eq!(
        command.args[command.args.len() - 2..],
        ["follow".to_owned(), format!("{stale:x}")]
    );
    assert!(command.args.contains(&pile.display().to_string()));

    // A follower alive on this host holds the stale alarm: nothing to adopt.
    let held = Lock::acquire(locks.path(), stale).unwrap();
    let mut spawner = FakeSpawner::default();
    assert!(resume(
        &fixture.alarm,
        &mut clock,
        locks.path(),
        &mut spawner,
        &command_for(pile)
    )
    .unwrap()
    .is_empty());
    assert!(spawner.spawned.is_empty());
    drop(held);
}

#[test]
fn the_alarm_follower_command_is_a_capped_user_unit_with_absolute_paths_and_routes() {
    let alarm = Id::new([0xab; 16]).unwrap();
    let env = follower_env(
        &settings(),
        [
            ("TRIBLESPACE_COLLECTION_ALARM".to_owned(), "0123".to_owned()),
            ("TRIBLESPACE_KEY".to_owned(), "/home/x/key".to_owned()),
            ("PERSONA".to_owned(), "cyan".to_owned()),
            ("PATH".to_owned(), "/usr/bin".to_owned()),
        ],
    );
    assert!(env.contains(&("TRIBLESPACE_COLLECTION_ALARM".to_owned(), "0123".to_owned())));
    assert!(env.contains(&("ALARM_JP_DISCORD_USER".to_owned(), JP.to_owned())));
    assert!(env.contains(&("ALARM_MAC_SSH".to_owned(), "mac".to_owned())));
    assert!(env
        .iter()
        .all(|(name, _)| !["TRIBLESPACE_KEY", "PERSONA", "PATH"].contains(&name.as_str())));
    let command = follower_command(
        Path::new("/opt/cohort/alarm"),
        Path::new("/srv/self.pile"),
        Path::new("/srv/self.key"),
        alarm,
        env,
    )
    .unwrap();
    let args = command.systemd_run_args();
    let head: Vec<&str> = args.iter().take(8).map(String::as_str).collect();
    assert_eq!(
        head,
        [
            "--user",
            "--unit",
            "alarm-abababab",
            "--collect",
            "-p",
            "MemoryMax=1G",
            "-p",
            "OOMScoreAdjust=500"
        ]
    );
    assert!(args.contains(&"TRIBLESPACE_COLLECTION_ALARM=0123".to_owned()));
    let tail = &args[args.iter().position(|arg| arg == "--").unwrap()..];
    assert_eq!(
        tail,
        [
            "--",
            "/opt/cohort/alarm",
            "--pile",
            "/srv/self.pile",
            "--key",
            "/srv/self.key",
            "follow",
            "abababababababababababababababab"
        ]
    );
    assert!(follower_command(
        Path::new("alarm"),
        Path::new("/srv/p"),
        Path::new("/srv/k"),
        alarm,
        Vec::new()
    )
    .is_err());
    assert!(follower_command(
        Path::new("/opt/alarm"),
        Path::new("self.pile"),
        Path::new("/srv/k"),
        alarm,
        Vec::new()
    )
    .is_err());
}
