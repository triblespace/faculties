//! A private fixture with named parents, unfinished work, an unread settlement,
//! an explicit request for JP and a roster member with no current evidence.
#![cfg(feature = "widgets")]

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use ed25519_dalek::SigningKey;
use egui::epaint::Shape;
use faculties::storage::CollectionRoutes;
use faculties::viewer::{compose_hosted, Host, HostAction, Target};
use faculties::widgets::storage::WorkspaceObservation;
use faculties::widgets::WidgetContext;
use faculties::{clock, goals, message, relations, schemas, spawn};
use hifitime::Epoch;
use triblespace::core::collection::observed_store::DependencyTracker;
use triblespace::core::collection::{AdmissionPolicy, CollectionPolicy, CollectionStoreExt};
use triblespace::core::metadata;
use triblespace::core::repo::pile::Pile;
use triblespace::core::repo::{SnapshotSource, StoreChanges, StoreSnapshot};
use triblespace::prelude::*;
use GORBIE::prelude::CardCtx;

struct Fixture {
    _file: tempfile::NamedTempFile,
    pile: Pile,
    signer: SigningKey,
    routes: CollectionRoutes,
    child: String,
    ended_child: String,
    title_hash: [u8; 32],
    spawner: Id,
}

impl Fixture {
    fn new() -> Self {
        let file = tempfile::NamedTempFile::new().unwrap();
        let signer = SigningKey::from_bytes(&[19; 32]);
        let mut pile = Pile::open_as(file.path(), signer.verifying_key()).unwrap();
        let mut collection = |label| {
            pile.collection(
                label,
                CollectionPolicy::new(AdmissionPolicy::Open, AdmissionPolicy::Open),
            )
            .unwrap()
        };
        let runs = collection("Worker runs fixture");
        let goals = collection("Goals fixture");
        let messages = collection("Messages fixture");
        let people = collection("People fixture");
        let routes = CollectionRoutes::from([
            (schemas::spawn::DEFAULT_SCOPE_ID, runs.handle()),
            (schemas::compass::DEFAULT_SCOPE_ID, goals.handle()),
            (schemas::message::DEFAULT_SCOPE_ID, messages.handle()),
            (schemas::relations::DEFAULT_SCOPE_ID, people.handle()),
        ]);
        let rowan = genid().id;
        let iris = genid().id;
        let ash = genid().id;
        let child_id = genid().id;
        let ended_id = genid().id;
        let time = |minute| {
            clock::point(Epoch::from_gregorian_utc(2026, 11, 18, 8, minute, 0, 0)).unwrap()
        };
        let (mut observations, task) = goals::goal_fragment(
            "Inspect the rehearsal audio",
            vec!["Rowan".into()],
            None,
            time(0),
        )
        .unwrap();
        observations += goals::status_fragment(task, "doing", Some(rowan), time(1)).unwrap();
        let (mut request, ask) = goals::goal_fragment(
            "Confirm the studio date",
            vec!["Iris".into(), "ask-jp".into()],
            None,
            time(2),
        )
        .unwrap();
        request += goals::status_fragment(ask, "blocked", Some(iris), time(3)).unwrap();
        observations += goals::note_fragment(
            task,
            "Listening to track two",
            vec![],
            vec![],
            vec![],
            Some(rowan),
            time(4),
        )
        .unwrap()
        .0;
        request += goals::note_fragment(
            ask,
            "The room booking needs a date",
            vec![],
            vec![],
            vec![],
            Some(iris),
            time(5),
        )
        .unwrap()
        .0;
        pile.commit(goals, &signer, observations + request).unwrap();

        let mut workers = Fragment::empty();
        let mut mail = Fragment::empty();
        let mut names = vec![];
        for (index, (brief, persona)) in [
            ("Inspect the first synthetic track", child_id),
            ("Check the second synthetic track", ended_id),
        ]
        .into_iter()
        .enumerate()
        {
            let (declaration, spawn_id, _, name) = spawn::spawn_fragment(brief).unwrap();
            workers += declaration;
            workers += spawn::goal_fragment(spawn_id, task, &[]);
            let (start, run) = spawn::run_fragment(&spawn::RunFacts {
                spawn: spawn_id,
                started_at: time(4 + index as u8),
                persona,
                spawner: rowan,
                unit: format!("fixture-{index}.service"),
                workdir: "/fixture".into(),
                effort: "medium".into(),
                prompt: brief.into(),
                wake: None,
                provider: "fixture".into(),
                provider_reason: "testing".into(),
                model: None,
                model_unset: None,
                untinted: None,
                resumes: None,
                cover: None,
                resident: false,
            });
            workers += start;
            if index == 1 {
                workers += spawn::settlement_fragment(
                    run,
                    time(7),
                    &spawn::Exit {
                        code: Some(0),
                        ..Default::default()
                    },
                    Some("Complete"),
                    None,
                    &Default::default(),
                )
                .0;
                let (envelope, message_id) = message::message_fragment(
                    persona,
                    &message::Recipient::Person(rowan),
                    "Second track checked",
                    time(8),
                );
                mail += envelope;
                workers += spawn::run_annotation(run, Some(message_id), None, None);
            }
            names.push(name);
        }
        mail += message::message_fragment(
            rowan,
            &message::Recipient::Person(iris),
            "I am checking the rehearsal audio",
            time(1),
        )
        .0;
        mail += message::message_fragment(
            iris,
            &message::Recipient::Person(rowan),
            "The studio needs a date",
            time(3),
        )
        .0;
        // A historical statement in a message is not a present-tense activity record.
        mail += message::message_fragment(
            ash,
            &message::Recipient::Person(rowan),
            "I am idle during the interval",
            clock::point(Epoch::from_gregorian_utc(2025, 1, 10, 17, 30, 0, 0)).unwrap(),
        )
        .0;
        let mut roster = Fragment::empty();
        for (id, label) in [
            (rowan, "Rowan"),
            (iris, "Iris"),
            (ash, "Ash"),
            (child_id, &names[0]),
            (ended_id, &names[1]),
        ] {
            roster += relations::person_fragment(
                id,
                relations::ProfileInput {
                    label: label.into(),
                    affinities: vec![relations::ZOOID_AFFINITY.into()],
                    ..Default::default()
                },
            )
            .unwrap()
            .0;
        }
        pile.commit(people, &signer, roster).unwrap();
        pile.commit(messages, &signer, mail).unwrap();
        pile.commit(runs, &signer, workers).unwrap();
        let blob: Blob<blobencodings::UTF8String> = "Inspect the rehearsal audio".to_blob();
        let title_hash = blob.get_handle().raw;
        Self {
            _file: file,
            pile,
            signer,
            routes,
            child: names[0].clone(),
            ended_child: names[1].clone(),
            title_hash,
            spawner: rowan,
        }
    }

    fn observe(&mut self, serial: u64) -> (WorkspaceObservation, DependencyTracker) {
        let tracker = DependencyTracker::default();
        let view = pollster::block_on(WorkspaceObservation::observe_tracked(
            &mut self.pile,
            &self.signer,
            &self.routes,
            tracker.clone(),
        ))
        .unwrap()
        .with_presentation_revision(serial);
        (view, tracker)
    }
}

#[derive(Clone)]
struct FixtureHost {
    observation: Rc<RefCell<WorkspaceObservation>>,
    open: Rc<Cell<bool>>,
}

impl Host for FixtureHost {
    fn with_context(
        &self,
        card: &mut CardCtx<'_>,
        render: impl FnOnce(
            &mut CardCtx<'_>,
            WidgetContext<'_>,
            u64,
            &mut dyn FnMut(HostAction) -> Result<(), String>,
        ),
    ) {
        let ui = card.ui_mut();
        let id = ui.make_persistent_id("Colony");
        ui.ctx()
            .data_mut(|data| data.insert_persisted(id, self.open.get()));
        let observation = self.observation.borrow();
        render(card, observation.loaded_context(), 1, &mut |_| {
            panic!("read-only colony requested an action")
        });
    }
}

fn shape_text(shape: &Shape, result: &mut Vec<String>) {
    match shape {
        Shape::Text(t) => result.push(t.galley.text().to_owned()),
        Shape::Vec(v) => v.iter().for_each(|shape| shape_text(shape, result)),
        _ => {}
    }
}

#[test]
fn routed_colony_reads_on_open_and_replaces_stale_results() {
    let mut fixture = Fixture::new();
    let (first, tracker) = fixture.observe(1);
    let baseline = fixture.pile.snapshot().unwrap();
    let host = FixtureHost {
        observation: Rc::new(RefCell::new(first)),
        open: Rc::new(Cell::new(false)),
    };
    let ctx = egui::Context::default();
    let nb_host = host.clone();
    let mut notebook = GORBIE::NotebookConfig::new("Test dashboard")
        .without_editor()
        .embedded(&ctx, move |nb| {
            compose_hosted(nb, Target::Colony, nb_host.clone())
        });
    let mut frame = || {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1100.0, 1600.0),
            )),
            ..Default::default()
        };
        let output = ctx.run_ui(input, |ui| notebook.show(ui, None));
        let mut text = vec![];
        for shape in output.shapes {
            shape_text(&shape.shape, &mut text);
        }
        text.join("\n")
    };
    let read = |tracker: &DependencyTracker| {
        tracker
            .lock()
            .unwrap()
            .blobs
            .iter()
            .any(|hash| hash.raw == fixture.title_hash)
    };
    assert!(!read(&tracker));
    assert!(frame().contains("Colony"));
    assert!(
        !read(&tracker),
        "a closed section must not start the projection"
    );
    host.open.set(true);
    let wait = |frame: &mut dyn FnMut() -> String, wanted: &str| {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let text = frame();
            if text.contains(wanted) {
                return text;
            }
            assert!(Instant::now() < deadline, "missing {wanted:?} in {text:?}");
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    let text = wait(&mut frame, &fixture.child);
    for expected in [
        "Rowan",
        "Iris",
        "Ash",
        "Inspect the rehearsal audio",
        "Confirm the studio date",
        &fixture.ended_child,
        "unknown",
        "2025-01-10",
        "Latest note",
        "unread",
        "Started · current run state unknown",
        "Run ended · exit 0",
        "Assigned goal",
    ] {
        assert!(text.contains(expected), "missing {expected:?} in {text:?}");
    }
    assert!(!text.to_lowercase().contains("idle"));
    assert_eq!(text.matches("Relations roster").count(), 3);
    assert!(read(&tracker));
    tracker.lock().unwrap().blobs = Default::default();
    frame();
    assert!(
        !read(&tracker),
        "a stable open observation reuses its result"
    );
    host.open.set(false);
    let runs = faculties::collection_names::open_workspace_in(
        &baseline,
        fixture.routes[&schemas::spawn::DEFAULT_SCOPE_ID],
    )
    .unwrap();
    let (mut appended, worker, _, name) =
        spawn::spawn_fragment("A new fixture brief after the observation").unwrap();
    appended += spawn::run_fragment(&spawn::RunFacts {
        spawn: worker,
        started_at: clock::point_now().unwrap(),
        persona: genid().id,
        spawner: fixture.spawner,
        unit: "fixture-later.service".into(),
        workdir: "/fixture".into(),
        effort: "medium".into(),
        prompt: "fixture".into(),
        wake: None,
        provider: "fixture".into(),
        provider_reason: "testing".into(),
        model: None,
        model_unset: None,
        untinted: None,
        resumes: None,
        cover: None,
        resident: false,
    })
    .0;
    // A request of the removed host queue that nothing ever answered, as it
    // was written then: history, which this view does not list.
    let (asked, zooid, _, unanswered) =
        spawn::spawn_fragment("A fixture brief once asked of a host").unwrap();
    appended += asked;
    let provider = appended.put::<blobencodings::UTF8String, _>("fixture".to_owned());
    let effort = appended.put::<blobencodings::UTF8String, _>("medium".to_owned());
    let label = appended.put::<blobencodings::UTF8String, _>("studio fixture".to_owned());
    let core = entity! { _ @
        metadata::tag: schemas::spawn::KIND_REQUEST,
        schemas::spawn::spawn::zooid: zooid,
        schemas::spawn::spawn::host: Inline::new(fixture.signer.verifying_key().to_bytes()),
        schemas::spawn::spawn::provider: provider,
        schemas::spawn::spawn::effort: effort,
        metadata::created_at: clock::point_now().unwrap(),
    };
    let request = core.root().unwrap();
    appended += core;
    appended += entity! { ExclusiveId::force_ref(&request) @
        schemas::archive::archive::author: fixture.spawner,
        schemas::spawn::spawn::host_label: label,
    };
    fixture
        .pile
        .commit(runs, &fixture.signer, appended)
        .unwrap();
    assert_ne!(
        fixture
            .pile
            .snapshot()
            .unwrap()
            .changes_for(&baseline, &tracker.lock().unwrap()),
        StoreChanges::NONE,
        "a new Spawn commit must wake the resident viewer"
    );
    let (changed, _) = fixture.observe(2);
    *host.observation.borrow_mut() = changed;
    frame();
    host.open.set(true);
    let updated = wait(&mut frame, &name);
    for past in [unanswered.as_str(), "studio fixture", "Requests without"] {
        assert!(
            !updated.contains(past),
            "a past request is not work waiting: {past} in {updated:?}"
        );
    }
    host.open.set(false);
    fixture.routes.remove(&schemas::spawn::DEFAULT_SCOPE_ID);
    let (partial, _) = fixture.observe(3);
    *host.observation.borrow_mut() = partial;
    frame();
    host.open.set(true);
    let first = frame();
    assert!(first.contains("Spawn route missing"));
    let remaining = wait(&mut frame, "Inspect the rehearsal audio");
    assert!(remaining.contains("Spawn route missing"));
    // Relations still names these people. Only Spawn supplied their runs.
    assert!(remaining.contains(&fixture.child));
    for frame in [&first, &remaining] {
        for stale in ["Run ended", "Started ·", "Assigned goal", &name] {
            assert!(!frame.contains(stale), "stale Spawn evidence: {stale}");
        }
    }
    fixture.pile.close().unwrap();
}

#[test]
fn resident_observation_defers_an_unreadable_spawn_payload_until_requested() {
    use faculties::widgets::SourceKey;
    use triblespace::core::collection::{
        empty_metadata_handle, CollectionCommit, CollectionRecord, CollectionStore,
    };
    use triblespace::prelude::inlineencodings::Handle;

    let mut fixture = Fixture::new();
    let root = fixture.routes[&schemas::spawn::DEFAULT_SCOPE_ID];
    let withheld: Blob<blobencodings::SimpleArchive> = entity! { _ @ metadata::name: "withheld" }
        .into_facts()
        .to_blob();
    fixture
        .pile
        .insert(CollectionRecord::Commit(CollectionCommit::sign(
            &fixture.signer,
            root,
            Handle::<blobencodings::SimpleArchive>::to_hash(withheld.get_handle()),
            empty_metadata_handle(),
        )))
        .unwrap();
    let (observed, _) = fixture.observe(1);
    let context = observed.loaded_context();
    assert!(
        context.contains(SourceKey::Spawn),
        "the closed viewer freezes a routed selection before reading its archive"
    );
    assert!(
        context.dataset(SourceKey::Spawn).is_none(),
        "opening must surface a missing archive instead of partial evidence"
    );
    assert!(context.dataset(SourceKey::Goals).is_some());
    fixture.pile.close().unwrap();
}

#[test]
#[ignore = "uses a native graphics device to export the named synthetic fixture"]
fn export_colony_fixture() {
    let directory = std::path::PathBuf::from(
        std::env::var_os("COLONY_FIXTURE_OUTPUT").expect("fixture output directory"),
    );
    let mut fixture = Fixture::new();
    let (observation, _) = fixture.observe(1);
    let host = FixtureHost {
        observation: Rc::new(RefCell::new(observation)),
        open: Rc::new(Cell::new(true)),
    };
    let mut pages = 0;
    GORBIE::NotebookConfig::new("Colony fixture")
        .without_editor()
        .capture(
            GORBIE::CaptureOptions {
                pixels_per_point: 1.5,
                settle_timeout: Duration::from_secs(2),
            },
            move |nb| compose_hosted(nb, Target::Colony, host.clone()),
            |image| {
                std::fs::write(directory.join(image.filename()), image.bytes)?;
                pages += 1;
                Ok(())
            },
        )
        .unwrap();
    assert!(pages > 0);
    fixture.pile.close().unwrap();
}
