//! The Avalanche viewer in the ordinary Data composition, over a disposable
//! pile with several workspaces: one holding the colony's seed rules and a
//! conclusion the engine itself derived, one quiet (selected rules, nothing
//! concluded), one with no program at all, one whose Rules bytes are missing,
//! and one without a Rules route. Opening and navigating the viewer must leave
//! the pile's bytes unchanged.
#![cfg(feature = "widgets")]

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use ed25519_dalek::SigningKey;
use egui::epaint::Shape;
use faculties::schemas::rules::{
    rules as rule_attrs, ASK_JP_REVISION, ENVIRONMENT_ANCHOR, IGNITE_REVISION,
};
use faculties::storage::CollectionRoutes;
use faculties::viewer::{compose_hosted, Host, HostAction, Target};
use faculties::widgets::rules::web;
use faculties::widgets::storage::WorkspaceObservation;
use faculties::widgets::WidgetContext;
use faculties::{quota, rules, schemas};
use triblespace::core::collection::{
    empty_metadata_handle, AdmissionPolicy, CollectionCommit, CollectionPolicy, CollectionRecord,
    CollectionStore, CollectionStoreExt,
};
use triblespace::core::metadata;
use triblespace::core::reactive::collection::schema as environment;
use triblespace::core::reactive::{text, Environment, Progress};
use triblespace::core::repo::pile::Pile;
use triblespace::prelude::inlineencodings::Handle;
use triblespace::prelude::*;
use GORBIE::prelude::CardCtx;

struct Fixture {
    file: tempfile::NamedTempFile,
    pile: Pile,
    signer: SigningKey,
    /// Workspaces: full, quiet, empty, unreadable, unrouted.
    workspaces: Vec<CollectionRoutes>,
    goal: Id,
    request: Id,
    tick: Id,
    nest: Id,
}

fn name(fragment: &mut Fragment, id: Id, label: &str) {
    let handle = fragment.put::<blobencodings::UTF8String, _>(label.to_owned());
    *fragment += entity! { ExclusiveId::force_ref(&id) @ metadata::name: handle };
}

/// Two contrast rules as text: a reversed edge that feeds itself over a
/// finite set, and a nesting rule whose every result is a new identity.
fn contrast_program() -> (Fragment, Id, Id, Id, Id) {
    let ids: Vec<Id> = (0..16).map(|_| genid().id).collect();
    let [edge, node, wraps, reverse, nest] = [ids[0], ids[1], ids[2], ids[3], ids[4]];
    let source = format!(
        "alias edge = id\"{edge:X}\";\n\
         alias node = id\"{node:X}\";\n\
         alias wraps = id\"{wraps:X}\";\n\
         alias reverse = id\"{reverse:X}\";\n\
         alias nest = id\"{nest:X}\";\n\
         alias x = id\"{:X}\"; alias y = id\"{:X}\";\n\
         alias p = id\"{:X}\"; alias k = id\"{:X}\"; alias n = id\"{:X}\";\n\
         alias m1 = id\"{:X}\"; alias e1 = id\"{:X}\";\n\
         alias m2 = id\"{:X}\"; alias c2 = id\"{:X}\"; alias e2 = id\"{:X}\";\n\
         rule @reverse {{\n\
           operation POSITIVE;\n\
           matches @m1 (?x @edge ?y);\n\
           emits @e1 (?y @edge ?x);\n\
         }}\n\
         rule @nest {{\n\
           operation POSITIVE;\n\
           matches @m2 (?p @node ?k);\n\
           core @c2 (?n @wraps ?p);\n\
           emits @e2 (?n @node ?k);\n\
         }}\n",
        ids[5], ids[6], ids[7], ids[8], ids[9], ids[10], ids[11], ids[12], ids[13], ids[14]
    );
    let program = text::parse(source.into_bytes().into()).unwrap();
    (program, edge, node, reverse, nest)
}

impl Fixture {
    fn new() -> Self {
        let file = tempfile::NamedTempFile::new().unwrap();
        let signer = SigningKey::from_bytes(&[23; 32]);
        let mut pile = Pile::open_as(file.path(), signer.verifying_key()).unwrap();
        let mut collection = |label| {
            pile.collection(
                label,
                CollectionPolicy::new(AdmissionPolicy::Open, AdmissionPolicy::Open),
            )
            .unwrap()
        };
        let rules_full = collection("Rules fixture A");
        let schemas_full = collection("Schemas fixture A");
        let rules_quiet = collection("Rules fixture B");
        let rules_empty = collection("Rules fixture C");
        let rules_missing = collection("Rules fixture D");
        let schemas_other = collection("Schemas fixture E");

        // Workspace A: the colony's seed program, one tick, and exactly the
        // facts the engine concludes from them, committed as a node would.
        let program = rules::program().unwrap();
        let goal = genid().id;
        let sensed = rules::Sensed {
            eligible: vec![goal],
            quota_ok: Some(genid().id),
            ..Default::default()
        };
        let tick =
            rules::tick_fragment(quota::unix_point(1_800_000_000).unwrap(), &sensed).unwrap();
        let tick_id = tick.root().unwrap();
        let mut engine = Environment::new();
        engine.extend(program.facts());
        engine.extend(tick.facts());
        engine.select([IGNITE_REVISION, ASK_JP_REVISION]);
        let mut concluded = Fragment::empty();
        for round in 0.. {
            assert!(round < 8, "the seed program did not settle");
            let advance = engine.advance(64).unwrap();
            for firing in advance.firings {
                concluded += firing.emitted;
            }
            if advance.progress == Progress::Quiescent {
                break;
            }
        }
        let request = *rules::run_requests_in(concluded.facts(), tick_id)
            .keys()
            .next()
            .expect("the engine concluded one run request");
        pile.commit(rules_full, &signer, program).unwrap();
        pile.commit(rules_full, &signer, tick).unwrap();
        pile.commit(rules_full, &signer, concluded).unwrap();
        let mut names = Fragment::empty();
        name(&mut names, IGNITE_REVISION, "ignite");
        name(&mut names, rule_attrs::run_for.id(), "run_for");
        name(&mut names, rule_attrs::eligible.id(), "eligible");
        pile.commit(schemas_full, &signer, names).unwrap();

        // Workspace B: two selected contrast rules and one seed fact each,
        // never evaluated: quiet, with nothing concluded.
        let (contrast, edge, node, reverse, nest) = contrast_program();
        let (a, b, seed, kind) = (genid().id, genid().id, genid().id, genid().id);
        let mut quiet = contrast;
        quiet += entity! { _ @
            environment::reading_of: ENVIRONMENT_ANCHOR,
            environment::selects*: [reverse, nest],
        };
        let edge_fact: Inline<GenId> = b.to_inline();
        let node_fact: Inline<GenId> = kind.to_inline();
        let mut seeds = TribleSet::new();
        seeds.insert(&Trible::force(&a, &edge, &edge_fact));
        seeds.insert(&Trible::force(&seed, &node, &node_fact));
        let mut quiet_facts = quiet.facts().clone();
        quiet_facts += seeds.clone();
        pile.commit(rules_quiet, &signer, quiet).unwrap();
        pile.commit(rules_quiet, &signer, Fragment::from(seeds))
            .unwrap();
        // The contrast controls, checked against the engine itself: the
        // reversed edge settles, the nesting rule keeps constructing.
        let mut reversed = Environment::new();
        reversed.extend(&quiet_facts);
        reversed.select([reverse]);
        let settled =
            (0..16).any(|_| reversed.advance(1_000).unwrap().progress == Progress::Quiescent);
        assert!(settled, "the reversed edge settles on a finite set");
        let mut nesting = Environment::new();
        nesting.extend(&quiet_facts);
        nesting.select([nest]);
        for _ in 0..4 {
            let grown = nesting.advance(25).unwrap();
            assert_eq!(
                grown.progress,
                Progress::Pending,
                "the nesting rule keeps constructing"
            );
            assert!(!grown.firings.is_empty());
        }

        // Workspace C: a sensor tick, but no program and no reading.
        let lone = rules::tick_fragment(
            quota::unix_point(1_800_000_100).unwrap(),
            &Default::default(),
        )
        .unwrap();
        pile.commit(rules_empty, &signer, lone).unwrap();

        // Workspace D: a commit whose archive bytes this pile never received.
        let withheld: Blob<blobencodings::SimpleArchive> =
            entity! { _ @ metadata::name: "withheld" }
                .into_facts()
                .to_blob();
        pile.insert(CollectionRecord::Commit(CollectionCommit::sign(
            &signer,
            rules_missing,
            Handle::<blobencodings::SimpleArchive>::to_hash(withheld.get_handle()),
            empty_metadata_handle(),
        )))
        .unwrap();

        let rules_scope = schemas::rules::DEFAULT_SCOPE_ID;
        let schemas_scope = schemas::atlas::DEFAULT_SCOPE_ID;
        let workspaces = vec![
            CollectionRoutes::from([
                (rules_scope, rules_full.handle()),
                (schemas_scope, schemas_full.handle()),
            ]),
            CollectionRoutes::from([(rules_scope, rules_quiet.handle())]),
            CollectionRoutes::from([(rules_scope, rules_empty.handle())]),
            CollectionRoutes::from([(rules_scope, rules_missing.handle())]),
            // Schemas only: without a Rules route nothing may be borrowed
            // from workspace A.
            CollectionRoutes::from([(schemas_scope, schemas_other.handle())]),
        ];
        Self {
            file,
            pile,
            signer,
            workspaces,
            goal,
            request,
            tick: tick_id,
            nest,
        }
    }

    fn observe(&mut self, workspace: usize, serial: u64) -> WorkspaceObservation {
        pollster::block_on(WorkspaceObservation::observe(
            &mut self.pile,
            &self.signer,
            &self.workspaces[workspace],
        ))
        .unwrap()
        .with_presentation_revision(serial)
    }

    fn bytes(&self) -> Vec<u8> {
        std::fs::read(self.file.path()).unwrap()
    }
}

/// A workspace host as the app's: it renders the frozen observation of the
/// selected workspace and its selection generation; any action is a failure.
#[derive(Clone)]
struct FixtureHost {
    observation: Rc<RefCell<WorkspaceObservation>>,
    generation: Rc<Cell<u64>>,
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
        let id = ui.make_persistent_id(faculties::widgets::rules::SECTION);
        ui.ctx().data_mut(|data| data.insert_persisted(id, true));
        let observation = self.observation.borrow();
        render(
            card,
            observation.loaded_context(),
            self.generation.get(),
            &mut |_| panic!("the read-only rules viewer requested an action"),
        );
    }
}

fn texts(shape: &Shape, result: &mut Vec<(String, egui::Rect)>) {
    match shape {
        Shape::Text(t) => result.push((t.galley.text().to_owned(), t.visual_bounding_rect())),
        Shape::Vec(v) => v.iter().for_each(|shape| texts(shape, result)),
        _ => {}
    }
}

struct Screen {
    ctx: egui::Context,
    notebook: GORBIE::Notebook,
    click: Option<egui::Pos2>,
    last: Vec<(String, egui::Rect)>,
}

impl Screen {
    fn frame(&mut self) -> String {
        let mut events = Vec::new();
        if let Some(pos) = self.click.take() {
            events.push(egui::Event::PointerMoved(pos));
            events.push(egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: Default::default(),
            });
            events.push(egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: Default::default(),
            });
        }
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1200.0, 6000.0),
            )),
            events,
            ..Default::default()
        };
        let notebook = &mut self.notebook;
        let output = self.ctx.run_ui(input, |ui| notebook.show(ui, None));
        let mut found = vec![];
        for shape in output.shapes {
            texts(&shape.shape, &mut found);
        }
        self.last = found;
        self.last
            .iter()
            .map(|(t, _)| t.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn wait(&mut self, wanted: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let text = self.frame();
            if text.contains(wanted) {
                return text;
            }
            assert!(Instant::now() < deadline, "missing {wanted:?} in {text:?}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn wait_any(&mut self, wanted: &[&str]) -> String {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let text = self.frame();
            if wanted.iter().any(|w| text.contains(w)) {
                return text;
            }
            assert!(Instant::now() < deadline, "missing {wanted:?} in {text:?}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Click the first drawn text exactly equal to `label`, as a user would.
    fn click(&mut self, label: &str) {
        let rect = self
            .last
            .iter()
            .find(|(text, _)| text == label)
            .unwrap_or_else(|| panic!("no {label:?} to click"))
            .1;
        self.click = Some(rect.center());
        self.frame();
        self.frame();
    }
}

#[test]
fn data_view_shows_the_selected_workspace_rules_and_writes_nothing() {
    let mut fixture = Fixture::new();
    let observations: Vec<WorkspaceObservation> =
        (0..5).map(|w| fixture.observe(w, w as u64 + 1)).collect();
    // The host's observations are done; from here on only the viewer runs.
    let before = fixture.bytes();
    let mut observations = observations.into_iter();
    let host = FixtureHost {
        observation: Rc::new(RefCell::new(observations.next().unwrap())),
        generation: Rc::new(Cell::new(1)),
    };
    let ctx = egui::Context::default();
    let nb_host = host.clone();
    // The ordinary Data composition: the app's window composes Dashboard.
    let notebook = GORBIE::NotebookConfig::new("Data")
        .without_editor()
        .embedded(&ctx, move |nb| {
            compose_hosted(nb, Target::Dashboard, nb_host.clone())
        });
    let mut screen = Screen {
        ctx,
        notebook,
        click: None,
        last: vec![],
    };

    let list = screen.wait("rule revisions");
    for expected in [
        "Avalanche rules",
        "Read-only: viewing evaluates, seeds and publishes nothing.",
        "selects 2 revisions",
        "2 rule revisions",
        "ignite",
        "selected by the current reading of 934a35da",
        "1 stored conclusion-shaped entity",
        "Rule web",
        "Whether an evaluator runs it is not recorded in the pile.",
    ] {
        assert!(list.contains(expected), "missing {expected:?} in {list:?}");
    }

    screen.click("ignite");
    let detail = screen.wait("Stored conclusion-shaped facts");
    let request = format!("{:x}", fixture.request);
    let goal = format!("{:x}", fixture.goal);
    let tick = format!("{:x}", fixture.tick);
    for expected in [
        format!("revision {:x}", IGNITE_REVISION),
        "Matches".to_owned(),
        "(?a @eligible ?c)".to_owned(),
        "Concludes (identity core)".to_owned(),
        "(?d @run_for ?c)".to_owned(),
        "Exact text projection".to_owned(),
        "not evidence that any action happened".to_owned(),
        format!("entity {}", &request[..8]),
        format!("run_for: {}", &goal[..8]),
        format!("linked input ?a = {}", &tick[..8]),
        format!("linked input ?c = {}", &goal[..8]),
        "not described in this Rules collection".to_owned(),
        "eligible:".to_owned(),
    ] {
        assert!(
            detail.contains(&expected),
            "missing {expected:?} in {detail:?}"
        );
    }
    for invented in ["fired at", "firing history", "evaluator ran", "last run"] {
        assert!(
            !detail.to_lowercase().contains(invented),
            "invented {invented:?}"
        );
    }
    screen.click("← All rules");
    screen.wait("2 rule revisions");

    // Quiet workspace: selection resets, nothing is borrowed from A.
    screen.click("ignite");
    screen.wait("Stored conclusion-shaped facts");
    *host.observation.borrow_mut() = observations.next().unwrap();
    host.generation.set(2);
    let quiet = screen.wait("rule revisions");
    for expected in [
        "Names unavailable",
        "selects 2 revisions",
        "can feed itself",
        "Static feedback cycle",
        "it settles whenever its inputs are finite",
        "it may settle or keep growing",
    ] {
        assert!(
            quiet.contains(expected),
            "missing {expected:?} in {quiet:?}"
        );
    }
    for leaked in ["ignite", &request[..8], "Stored conclusion-shaped facts"] {
        assert!(!quiet.contains(leaked), "workspace A leaked: {leaked:?}");
    }
    let nest = format!("rule {}", &format!("{:x}", fixture.nest)[..8]);
    screen.click(&nest);
    let opened = screen.wait("Stored conclusion-shaped facts");
    assert!(opened.contains("None stored yet."), "{opened:?}");
    assert!(opened.contains("can feed itself"), "{opened:?}");
    assert!(opened.contains("still needs") || opened.contains("no remaining conditions"));
    screen.click("← All rules");

    // Empty workspace: a tick, no program.
    *host.observation.borrow_mut() = observations.next().unwrap();
    host.generation.set(3);
    let empty = screen.wait("No rule revisions in this workspace's Rules collection.");
    assert!(empty.contains("No reading selects any rule in this workspace."));
    assert!(empty.contains("1 other entity"));

    // Missing bytes: the route exists, its collection cannot be read.
    *host.observation.borrow_mut() = observations.next().unwrap();
    host.generation.set(4);
    let missing = screen.wait_any(&[
        "Rules unavailable in this workspace.",
        "Rules evidence unavailable",
    ]);
    assert!(!missing.contains("rule revisions"));

    // No Rules route at all: unconfigured, never another workspace's rules.
    *host.observation.borrow_mut() = observations.next().unwrap();
    host.generation.set(5);
    let unrouted = screen.wait("Rules route missing from this workspace.");
    assert!(!unrouted.contains("ignite"));

    assert!(
        fixture.bytes() == before,
        "opening and navigating the viewer changed the pile"
    );
    fixture.pile.close().unwrap();
}

#[test]
fn the_web_finds_maximal_overlaps_and_remaining_conditions() {
    // The seed program's two rules: ignite concludes run_for/requested_in,
    // ask_jp concludes nudges; neither matches the other's conclusions.
    let program = rules::program().unwrap();
    let file = tempfile::NamedTempFile::new().unwrap();
    let signer = SigningKey::from_bytes(&[29; 32]);
    let mut pile = Pile::open_as(file.path(), signer.verifying_key()).unwrap();
    let root = pile
        .collection(
            "Rules web fixture",
            CollectionPolicy::new(AdmissionPolicy::Open, AdmissionPolicy::Open),
        )
        .unwrap();
    pile.commit(root, &signer, program).unwrap();
    let routes = CollectionRoutes::from([(schemas::rules::DEFAULT_SCOPE_ID, root.handle())]);
    let observation =
        pollster::block_on(WorkspaceObservation::observe(&mut pile, &signer, &routes)).unwrap();
    let context = observation.loaded_context();
    let view = faculties::widgets::rules::projection::project(
        context
            .dataset(faculties::widgets::SourceKey::Rules)
            .unwrap(),
        None,
    );
    assert_eq!(view.rules.len(), 2);
    let graph = web::web(&view.rules);
    assert!(graph.selected_only);
    assert!(graph.edges.is_empty(), "{:?}", graph.edges);
    assert!(graph.cycles.is_empty());
    pile.close().unwrap();
}
