//! The follower on a fake clock with a fake transport and a fake spawner:
//! no unit is started, no Discord call is made, nothing sounds.

use std::cell::RefCell;

use super::*;
use crate::alarm::ack::tests::message;
use crate::alarm::ack::Detected;
use crate::alarm::ladder::Due;
use crate::alarm::operations::tests::{fixture, request, FakeClock, T0};
use crate::alarm::operations::{commit_to, Raised};
use crate::alarm::operations::{FollowState, StepReport};
use crate::alarm::rungs::tests::{settings, FakeTransport};
use crate::alarm::rungs::Executor;
use crate::alarm::rungs::Subject;
use crate::alarm::MINUTE;
use crate::schemas::alarm::{RUNG_DISCORD_MENTION, VIA_DISCORD_REPLY};
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
    assert!(Lock::held(directory.path(), alarm).unwrap());
    assert_eq!(
        Lock::recorded_pid(directory.path(), alarm),
        Some(std::process::id())
    );
    let error = Lock::acquire(directory.path(), alarm).unwrap_err();
    assert!(format!("{error:#}").contains("already runs"), "{error:#}");
    // Another alarm has its own lock.
    let other = Lock::acquire(directory.path(), Id::new([0xcd; 16]).unwrap()).unwrap();
    drop(other);
    drop(first);
    // Released, never unlinked.
    assert!(Lock::path(directory.path(), alarm).exists());
    assert!(!Lock::held(directory.path(), alarm).unwrap());
    let taken = Lock::acquire(directory.path(), alarm).unwrap();
    assert!(Lock::held(directory.path(), alarm).unwrap());
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
    assert!(
        args.contains(&UNIT_RUNTIME_MAX.to_owned()),
        "the unit has a runtime backstop"
    );
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

#[test]
fn a_follower_lock_is_decided_by_a_held_flock_not_by_the_recorded_pid() {
    use std::os::unix::io::AsRawFd;
    let directory = tempfile::tempdir().unwrap();
    let alarm = Id::new([0xab; 16]).unwrap();
    let path = Lock::path(directory.path(), alarm);
    // A file naming a live process (this one) that holds no lock is free:
    // the pid is display, never a decision.
    std::fs::write(&path, format!("{}\n", std::process::id())).unwrap();
    let lock = Lock::acquire(directory.path(), alarm).expect("nobody holds the lock");
    drop(lock);
    // An empty file another open description holds locked is held.
    let holder = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&path)
        .unwrap();
    std::fs::write(&path, "").unwrap();
    // SAFETY: flock on a file descriptor this test owns.
    assert_eq!(
        unsafe { libc::flock(holder.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    assert!(Lock::acquire(directory.path(), alarm).is_err());
    drop(holder);
    // Its holder gone, the lock is free again, with no staleness rule.
    assert!(Lock::acquire(directory.path(), alarm).is_ok());
}

#[test]
fn alarm_follower_lock_racers_exactly_one_wins() {
    use std::sync::{Arc, Barrier};
    let directory = Arc::new(tempfile::tempdir().unwrap());
    let alarm = Id::new([0xab; 16]).unwrap();
    for _round in 0..20 {
        let start = Arc::new(Barrier::new(8));
        let done = Arc::new(Barrier::new(8));
        let racers: Vec<_> = (0..8)
            .map(|_| {
                let (directory, start, done) = (directory.clone(), start.clone(), done.clone());
                std::thread::spawn(move || {
                    start.wait();
                    let won = Lock::acquire(directory.path(), alarm);
                    // Every winner holds its lock until all have tried.
                    done.wait();
                    won.is_ok()
                })
            })
            .collect();
        let winners = racers
            .into_iter()
            .map(|racer| racer.join().unwrap())
            .filter(|won| *won)
            .count();
        assert_eq!(winners, 1);
    }
}

/// The real ledger, with each observation passed through `edit` first: a
/// stand-in for a read that could not see every selected commit.
struct Partial<'a, F: FnMut(&mut FollowState)> {
    inner: &'a crate::alarm::Alarm,
    edit: RefCell<F>,
}

impl<F: FnMut(&mut FollowState)> Ledger for Partial<'_, F> {
    fn subject(&self, alarm: Id) -> Result<Subject> {
        self.inner.subject(alarm)
    }

    fn state(&self, alarm: Id, jp_user: Option<&str>) -> Result<FollowState> {
        let mut state = self.inner.state(alarm, jp_user)?;
        (self.edit.borrow_mut())(&mut state);
        Ok(state)
    }

    fn record_detected(&self, alarm: Id, detected: Detected) -> Result<Option<Id>> {
        self.inner.record_detected(alarm, detected)
    }

    fn note(&self, alarm: Id, by: Id, text: &str, at: i128) -> Result<Id> {
        self.inner.note_as(alarm, by, text, at)
    }

    fn run_rungs(
        &self,
        subject: &Subject,
        due: &[Due],
        runner: &mut dyn RungRunner,
        clock: &mut dyn Clock,
    ) -> Result<Vec<StepReport>> {
        self.inner.run_rungs(subject, due, runner, clock)
    }
}

/// Commit a JP reply to the first mention.
fn reply(fixture: &crate::alarm::operations::tests::Fixture, at: i128) -> Result<()> {
    let reply = message(
        "100000000000000613",
        JP,
        "here",
        at,
        Some("100000000000000901"),
    );
    fixture
        .storage
        .with_store(|pile, signer, runtime| {
            commit_to(
                &fixture.storage,
                pile,
                signer,
                runtime,
                DISCORD_SCOPE_ID,
                "Discord",
                reply,
            )
        })
        .map(drop)
}

#[test]
fn an_incomplete_observation_that_hides_the_raise_does_not_end_the_follower_as_closed() {
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
    // The first three turns cannot see the raise: its commit is unread.
    let hidden = RefCell::new(0);
    let ledger = Partial {
        inner: &fixture.alarm,
        edit: RefCell::new(|state: &mut FollowState| {
            let mut hidden = hidden.borrow_mut();
            if *hidden < 3 {
                *hidden += 1;
                state.raised = false;
                state.steps.clear();
                state.unread += 1;
            }
        }),
    };
    let mut pull = || reply(&fixture, T0 + MINUTE);
    let ended = follow(&ledger, alarm, &mut runner, &mut clock, Some(JP), &mut pull).unwrap();
    assert_eq!(
        ended,
        Ended::Acked {
            via: VIA_DISCORD_REPLY
        }
    );
    // The follower pulls Discord while it cannot see the raise, so JP's
    // reply may be found (and win) while the raise is still hidden.
    assert!((1..=3).contains(&*hidden.borrow()));
    // Nothing ran while the raise was hidden: R0's two steps only.
    let shown = fixture.alarm.show(&format!("{alarm:x}")).unwrap();
    assert_eq!(shown.steps.len(), 2);
}

#[test]
fn an_incomplete_observation_that_hides_a_step_does_not_repeat_the_rung() {
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
    // Once R1's mention is recorded, the next three turns cannot see it.
    let hidden = std::cell::Cell::new(0);
    let ledger = Partial {
        inner: &fixture.alarm,
        edit: RefCell::new(|state: &mut FollowState| {
            let r1 = state.steps.iter().position(|step| {
                step.rung == RUNG_DISCORD_MENTION && step.started >= T0 + 3 * MINUTE
            });
            if let (Some(r1), true) = (r1, hidden.get() < 3) {
                hidden.set(hidden.get() + 1);
                state.steps.remove(r1);
                state.unread += 1;
            }
        }),
    };
    let mut pull = || {
        if hidden.get() >= 3 {
            reply(&fixture, T0 + 4 * MINUTE)?;
        }
        Ok(())
    };
    let ended = follow(&ledger, alarm, &mut runner, &mut clock, Some(JP), &mut pull).unwrap();
    assert_eq!(
        ended,
        Ended::Acked {
            via: VIA_DISCORD_REPLY
        }
    );
    assert_eq!(hidden.get(), 3);
    let re_mentions = runner
        .transport
        .calls
        .iter()
        .filter(|call| call.contains("after 3 min"))
        .count();
    assert_eq!(re_mentions, 1, "{:?}", runner.transport.calls);
}

#[test]
fn the_follower_is_given_the_workspace_and_routes_the_raise_used_not_the_ambient_ones() {
    let directory = tempfile::tempdir().unwrap();
    let pile = directory.path().join("self.pile");
    let key = directory.path().join("self.key");
    std::fs::write(&pile, b"").unwrap();
    std::fs::write(&key, b"").unwrap();
    let alarm = Id::new([0xab; 16]).unwrap();
    let program = Path::new("/opt/cohort/alarm");
    let ambient_v = Id::new([0x56; 16]).unwrap();
    let ambient = vec![
        ("FACULTIES_WORKSPACE".to_owned(), format!("{ambient_v:X}")),
        ("TRIBLESPACE_COLLECTION_ALARM".to_owned(), "bb".repeat(32)),
        ("TRIBLESPACE_PEERS".to_owned(), "peer".to_owned()),
    ];
    let names = |command: &FollowerCommand| -> Vec<String> {
        command.env.iter().map(|(name, _)| name.clone()).collect()
    };

    // The raise ran in workspace W while the environment named V.
    let signer = ed25519_dalek::SigningKey::from_bytes(&[7; 32]).verifying_key();
    let workspace = crate::workspace::Workspace::new("the raise's workspace");
    let w = workspace.id;
    let storage = crate::storage::Storage::new(pile.clone(), Some(key.clone()))
        .with_workspace(workspace, signer);
    let command = super::command_for(program, &storage, &settings(), &ambient, alarm).unwrap();
    let follow_at = command.args.iter().position(|arg| arg == "follow").unwrap();
    let flag = command
        .args
        .iter()
        .position(|arg| arg == "--workspace")
        .unwrap();
    assert!(flag < follow_at, "{:?}", command.args);
    assert_eq!(command.args[flag + 1], format!("{w:X}"));
    assert!(!command.args.contains(&format!("{ambient_v:X}")));
    assert!(!names(&command).contains(&"FACULTIES_WORKSPACE".to_owned()));
    assert!(!names(&command).contains(&"TRIBLESPACE_COLLECTION_ALARM".to_owned()));
    assert!(names(&command).contains(&"TRIBLESPACE_PEERS".to_owned()));

    // Explicit routes without a workspace travel as the whole map in
    // explicit --route flags, never as collection variables.
    let routed = crate::storage::Storage::new(pile.clone(), Some(key.clone())).with_collections(
        std::collections::BTreeMap::from([(
            crate::schemas::alarm::DEFAULT_SCOPE_ID,
            Inline::new([0xaa; 32]),
        )]),
    );
    let command = super::command_for(program, &routed, &settings(), &ambient, alarm).unwrap();
    assert!(!command.args.contains(&"--workspace".to_owned()));
    let route = command
        .args
        .iter()
        .position(|arg| arg == "--route")
        .unwrap();
    assert!(route < command.args.iter().position(|arg| arg == "follow").unwrap());
    assert_eq!(
        command.args[route + 1],
        format!("alarm={}", "aa".repeat(32))
    );
    assert!(!names(&command)
        .iter()
        .any(|name| name.starts_with("TRIBLESPACE_COLLECTION_")));
    assert!(!names(&command).contains(&"FACULTIES_WORKSPACE".to_owned()));

    // Legacy routing: the raise itself read the ambient collection
    // variables, so they are what the follower gets; still no selector.
    let legacy = crate::storage::Storage::new(pile, Some(key));
    let command = super::command_for(program, &legacy, &settings(), &ambient, alarm).unwrap();
    assert!(!command.args.contains(&"--workspace".to_owned()));
    assert!(command
        .env
        .contains(&("TRIBLESPACE_COLLECTION_ALARM".to_owned(), "bb".repeat(32))));
    assert!(!names(&command).contains(&"FACULTIES_WORKSPACE".to_owned()));
}

/// Hide every observation after the first `complete` turns (raise and steps
/// unseen, one commit unread) until turn `until`; the ack, if any, stays
/// visible.
fn hiding(
    complete: usize,
    until: usize,
    turns: &std::cell::Cell<usize>,
) -> impl FnMut(&mut FollowState) + '_ {
    move |state: &mut FollowState| {
        let turn = turns.get() + 1;
        turns.set(turn);
        if turn > complete && turn <= until {
            state.raised = false;
            state.steps.clear();
            state.unread += 1;
        }
    }
}

fn rungs_ran(fixture: &crate::alarm::operations::tests::Fixture, alarm: Id) -> Vec<(String, i128)> {
    fixture
        .alarm
        .show(&format!("{alarm:x}"))
        .unwrap()
        .steps
        .into_iter()
        .map(|step| (step.rung, step.started))
        .collect()
}

#[test]
fn past_the_grace_a_degraded_follower_escalates_from_its_last_complete_observation_once_per_rung() {
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
    // One complete observation, then incomplete ones for a long time (the
    // limit only keeps a follower that never acts from running forever).
    let turns = std::cell::Cell::new(0);
    let ledger = Partial {
        inner: &fixture.alarm,
        edit: RefCell::new(hiding(1, 400, &turns)),
    };
    // JP answers once the lights have breathed (R3, at 10 min).
    let mut pull = || {
        if rungs_ran(&fixture, alarm)
            .iter()
            .any(|(rung, _)| rung == "hue-alert")
        {
            reply(&fixture, T0 + 11 * MINUTE)?;
        }
        Ok(())
    };
    let ended = follow(&ledger, alarm, &mut runner, &mut clock, Some(JP), &mut pull).unwrap();
    // The ack wins although the observation that shows it is incomplete.
    assert_eq!(
        ended,
        Ended::Acked {
            via: VIA_DISCORD_REPLY
        }
    );
    assert!(turns.get() < 400, "acked while still degraded");
    let ran = rungs_ran(&fixture, alarm);
    let count = |rung: &str| ran.iter().filter(|(name, _)| name == rung).count();
    // Each due occurrence once, not once per degraded turn.
    assert_eq!(count("discord-mention"), 2, "{ran:?}");
    assert_eq!(count("mac-notify"), 1, "{ran:?}");
    assert_eq!(count("mac-sound"), 1, "{ran:?}");
    assert_eq!(count("reachy-shout"), 1, "{ran:?}");
    assert_eq!(count("hue-alert"), 1, "{ran:?}");
    // R1 ran on time, from the old observation: within the grace of R1's slot.
    let r1 = ran
        .iter()
        .filter(|(rung, _)| rung == "discord-mention")
        .map(|(_, at)| *at)
        .max()
        .unwrap();
    assert!(
        r1 >= T0 + 3 * MINUTE && r1 < T0 + 3 * MINUTE + 10 * SECOND,
        "R1 at {r1}"
    );
    let calls = &runner.transport.calls;
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.contains("'afplay'"))
            .count(),
        3
    );
    // And it said so in the ledger.
    let shown = fixture.alarm.show(&format!("{alarm:x}")).unwrap();
    assert!(
        shown.events.iter().any(|event| event.kind == "note"
            && event
                .text
                .as_deref()
                .is_some_and(|text| text.contains("acts on the observation from"))),
        "{:?}",
        shown.events
    );
}

#[test]
fn a_follower_that_never_saw_a_complete_observation_does_not_act() {
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
    // Incomplete from the first turn for five minutes of turns, then whole.
    let turns = std::cell::Cell::new(0);
    let ledger = Partial {
        inner: &fixture.alarm,
        edit: RefCell::new(hiding(0, 60, &turns)),
    };
    let mut pull = || {
        let mentions = rungs_ran(&fixture, alarm)
            .iter()
            .filter(|(rung, _)| rung == "discord-mention")
            .count();
        if mentions >= 2 {
            reply(&fixture, T0 + 6 * MINUTE)?;
        }
        Ok(())
    };
    let ended = follow(&ledger, alarm, &mut runner, &mut clock, Some(JP), &mut pull).unwrap();
    assert_eq!(
        ended,
        Ended::Acked {
            via: VIA_DISCORD_REPLY
        }
    );
    let r1 = rungs_ran(&fixture, alarm)
        .iter()
        .filter(|(rung, _)| rung == "discord-mention")
        .map(|(_, at)| *at)
        .max()
        .unwrap();
    assert!(
        r1 >= T0 + 5 * MINUTE,
        "nothing ran before a complete observation: R1 at {r1}"
    );
}

#[test]
fn a_follower_that_cannot_observe_stops_at_its_wall_cap_and_never_says_closed() {
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
    let turns = std::cell::Cell::new(0);
    let ledger = Partial {
        inner: &fixture.alarm,
        edit: RefCell::new(hiding(0, 1_600, &turns)),
    };
    let ended = follow(
        &ledger,
        alarm,
        &mut runner,
        &mut clock,
        Some(JP),
        &mut || Ok(()),
    )
    .unwrap();
    assert_eq!(ended, Ended::TimedOut);
    assert!(clock.now >= T0 + 120 * MINUTE && clock.now < T0 + 121 * MINUTE);
}
