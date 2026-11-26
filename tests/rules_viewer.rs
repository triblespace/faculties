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
use faculties::widgets::rules::projection::{self, ClauseKind, RulesView};
use faculties::widgets::rules::web;
use faculties::widgets::storage::WorkspaceObservation;
use faculties::widgets::{SourceKey, WidgetContext};
use faculties::{quota, rules, schemas};
use triblespace::core::collection::{
    empty_metadata_handle, AdmissionPolicy, CollectionCommit, CollectionPolicy, CollectionRecord,
    CollectionStore, CollectionStoreExt,
};
use triblespace::core::metadata;
use triblespace::core::reactive::collection::schema as environment;
use triblespace::core::reactive::schema as rule;
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
    assert!(
        !quiet.contains("termination is not stated"),
        "a complete scope states the sound termination fact: {quiet:?}"
    );
    let nest = format!("rule {}", &format!("{:x}", fixture.nest)[..8]);
    screen.click(&nest);
    let opened = screen.wait("Stored conclusion-shaped facts");
    assert!(opened.contains("None stored yet."), "{opened:?}");
    let itself = format!("{nest} can feed itself: 1 maximal overlap");
    assert!(opened.contains(&itself), "{opened:?}");
    // The edge's clauses show only when it is inspected.
    assert!(!opened.contains("overlap 1:"), "{opened:?}");
    screen.click(&itself);
    let inspected = screen.wait("overlap 1: 1 of 1 matches clause met");
    assert!(
        inspected.contains("no remaining conditions"),
        "{inspected:?}"
    );
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

/// Six rules over shared attributes, as text, so every overlap is known:
/// - `pair` constructs a pair from an edge and tags it with a literal kind;
/// - `labelled` meets three of pair's conclusions (one reused variable across
///   them, an equal literal), needs one more fact and a STRING_CONTAINS;
/// - `other_kind` wants a disjoint literal kind: no edge from `pair`;
/// - `fixed` wants a literal subject where `pair` constructs one at runtime;
/// - `refused` reads an unbound content term: the engine refuses it, so its
///   edges are structural only and the cycle through it is not feedback;
/// - `multi` has one clause with two predicates (both hold, as the engine
///   reads it) and feeds itself with a remaining condition.
struct WebProgram {
    program: Fragment,
    names: Fragment,
    rules: [Id; 6],
}

const WEB_RULES: [&str; 6] = [
    "pair",
    "labelled",
    "other_kind",
    "fixed",
    "refused",
    "multi",
];

const WEB_SOURCE: &str = r#"
rule @pair {
  operation POSITIVE;
  matches @p_m (?p_x @edge ?p_y);
  core @p_c1 (?p_n @pair_from ?p_x);
  core @p_c2 (?p_n @pair_to ?p_y);
  emits @p_e (?p_n @kind ?p_k);
  literal ?p_k genid(@pairkind);
}
rule @labelled {
  operation POSITIVE;
  matches @l_m1 (?l_a @pair_from ?l_s);
  matches @l_m2 (?l_a @pair_to ?l_t);
  matches @l_m3 (?l_a @kind ?l_k);
  matches @l_m4 (?l_t @label ?l_l);
  literal ?l_k genid(@pairkind);
  literal ?l_needle utf8("ok");
  constraint @l_c STRING_CONTAINS(?l_l, ?l_needle);
  emits @l_e (?l_t @flag ?l_a);
}
rule @other_kind {
  operation POSITIVE;
  matches @o_m (?o_b @kind ?o_k);
  literal ?o_k genid(@otherkind);
  emits @o_e (?o_b @flag ?o_b);
}
rule @fixed {
  operation POSITIVE;
  matches @f_m (?f_w @pair_from ?f_q);
  literal ?f_w genid(@someid);
  emits @f_e (?f_q @flag ?f_w);
}
rule @refused {
  operation POSITIVE;
  matches @r_m (?r_u @flag ?r_v);
  emits @r_e (?r_u @edge ?r_v);
  constraint @r_c STRING_CONTAINS(?r_v, ?r_nobody);
}
rule @multi {
  operation POSITIVE;
  matches @g_m (?g_g @label ?g_h);
  matches @g_m (?g_g @edge ?g_h);
  emits @g_e (?g_h @edge ?g_g);
}
"#;

fn web_program() -> WebProgram {
    // Every `@name` / `?name` in the source is an alias for a fresh id.
    let mut ids: Vec<(String, Id)> = Vec::new();
    for word in
        WEB_SOURCE.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '@' || c == '?'))
    {
        if let Some(alias) = word.strip_prefix('@').or_else(|| word.strip_prefix('?')) {
            if !ids.iter().any(|(a, _)| a == alias) {
                ids.push((alias.to_owned(), genid().id));
            }
        }
    }
    let id = |alias: &str| ids.iter().find(|(a, _)| a == alias).unwrap().1;
    let header: String = ids
        .iter()
        .map(|(alias, id)| format!("alias {alias} = id\"{id:X}\";\n"))
        .collect();
    let source = format!("{header}{WEB_SOURCE}");
    let program = text::parse(source.into_bytes().into()).unwrap();
    let mut names = Fragment::empty();
    for alias in WEB_RULES
        .iter()
        .chain(&["edge", "pair_from", "pair_to", "kind", "label", "flag"])
    {
        name(&mut names, id(*alias), alias);
    }
    WebProgram {
        program,
        names,
        rules: WEB_RULES.map(|alias| id(alias)),
    }
}

/// One workspace holding the web program, read by two current readings: one
/// selecting all six rules, one selecting `pair`, `other_kind` and a
/// revision whose body is not in this collection.
struct WebFixture {
    file: tempfile::NamedTempFile,
    pile: Pile,
    signer: SigningKey,
    routes: CollectionRoutes,
    rules: [Id; 6],
    second: Id,
    /// The program and readings exactly as committed, for engine controls.
    facts: TribleSet,
}

impl WebFixture {
    fn new() -> Self {
        let file = tempfile::NamedTempFile::new().unwrap();
        let signer = SigningKey::from_bytes(&[31; 32]);
        let mut pile = Pile::open_as(file.path(), signer.verifying_key()).unwrap();
        let policy = CollectionPolicy::new(AdmissionPolicy::Open, AdmissionPolicy::Open);
        let rules_root = pile.collection("Rules web fixture", policy).unwrap();
        let schemas_root = pile
            .collection(
                "Schemas web fixture",
                CollectionPolicy::new(AdmissionPolicy::Open, AdmissionPolicy::Open),
            )
            .unwrap();
        let WebProgram {
            mut program,
            names,
            rules,
        } = web_program();
        let (second, ghost) = (genid().id, genid().id);
        program += entity! { _ @
            environment::reading_of: ENVIRONMENT_ANCHOR,
            environment::selects*: rules,
        };
        program += entity! { _ @
            environment::reading_of: second,
            environment::selects*: [rules[0], rules[2], ghost],
        };
        let facts = program.facts().clone();
        pile.commit(rules_root, &signer, program).unwrap();
        pile.commit(schemas_root, &signer, names).unwrap();
        let routes = CollectionRoutes::from([
            (schemas::rules::DEFAULT_SCOPE_ID, rules_root.handle()),
            (schemas::atlas::DEFAULT_SCOPE_ID, schemas_root.handle()),
        ]);
        Self {
            file,
            pile,
            signer,
            routes,
            rules,
            second,
            facts,
        }
    }

    fn observe(&mut self) -> WorkspaceObservation {
        pollster::block_on(WorkspaceObservation::observe(
            &mut self.pile,
            &self.signer,
            &self.routes,
        ))
        .unwrap()
        .with_presentation_revision(1)
    }
}

fn rule_index(view: &RulesView, revision: Id) -> usize {
    view.rules
        .iter()
        .position(|rule| rule.revision == revision)
        .expect("listed")
}

fn reading_scope<'w>(graph: &'w web::Web, anchor: Id) -> &'w web::Scope {
    graph
        .scopes
        .iter()
        .find(|scope| scope.kind == web::ScopeKind::Reading(vec![anchor]))
        .expect("a scope per distinct reading")
}

#[test]
fn the_web_finds_maximal_overlaps_and_remaining_conditions() {
    // Negative control: the seed program's two rules (ignite concludes
    // run_for/requested_in, ask_jp concludes nudges) never match each other's
    // conclusions, and with nothing lost that absence may be said.
    {
        let program = rules::program().unwrap();
        let file = tempfile::NamedTempFile::new().unwrap();
        let signer = SigningKey::from_bytes(&[29; 32]);
        let mut pile = Pile::open_as(file.path(), signer.verifying_key()).unwrap();
        let root = pile
            .collection(
                "Rules seed fixture",
                CollectionPolicy::new(AdmissionPolicy::Open, AdmissionPolicy::Open),
            )
            .unwrap();
        pile.commit(root, &signer, program).unwrap();
        let routes = CollectionRoutes::from([(schemas::rules::DEFAULT_SCOPE_ID, root.handle())]);
        let observation =
            pollster::block_on(WorkspaceObservation::observe(&mut pile, &signer, &routes)).unwrap();
        let context = observation.loaded_context();
        let view = projection::project(context.dataset(SourceKey::Rules).unwrap(), None);
        assert_eq!(view.rules.len(), 2);
        let graph = web::web(&view);
        assert_eq!(graph.scopes.len(), 1);
        assert!(matches!(graph.scopes[0].kind, web::ScopeKind::Reading(_)));
        assert!(graph.scopes[0].complete());
        assert!(graph.edges.is_empty(), "{:?}", graph.edges);
        assert!(graph.scopes[0].cycles.is_empty());
        pile.close().unwrap();
    }

    let mut fixture = WebFixture::new();
    let observation = fixture.observe();
    let context = observation.loaded_context();
    let view = projection::project(
        context.dataset(SourceKey::Rules).unwrap(),
        context.dataset(SourceKey::Schemas),
    );
    let [pair, labelled, other_kind, fixed, refused, multi] =
        fixture.rules.map(|revision| rule_index(&view, revision));

    // Statuses, decided as the engine decides them, and checked against it.
    for (index, revision) in fixture.rules.iter().enumerate() {
        let mut engine = Environment::new();
        engine.extend(&fixture.facts);
        engine.select([*revision]);
        let outcome = engine.advance(8);
        assert_eq!(
            outcome.is_ok(),
            view.rules[rule_index(&view, *revision)].executable(),
            "{} disagrees with the engine: {outcome:?}",
            WEB_RULES[index]
        );
    }
    assert!(
        view.rules[refused]
            .refused
            .iter()
            .any(|cause| cause.contains("which no match or literal binds")),
        "{:?}",
        view.rules[refused].refused
    );
    // A valid multi-valued clause is read as both clauses, not refused.
    assert_eq!(view.rules[multi].clauses(ClauseKind::Matches).count(), 2);
    assert!(view.rules[multi].refused.is_empty());
    assert!(view.rules[multi].partial.is_empty());
    assert!(!view.rules[multi].notes.is_empty());
    assert_eq!(view.selected_missing.len(), 1);

    let graph = web::web(&view);
    assert_eq!(graph.scopes.len(), 2, "one scope per distinct reading");
    let all = reading_scope(&graph, ENVIRONMENT_ANCHOR);
    let second = reading_scope(&graph, fixture.second);
    assert_eq!(all.members.len(), 6);
    assert!(all.complete());
    assert_eq!(all.refused, vec![refused]);

    // A multi-clause overlap: three clauses meet three conclusions through one
    // reused variable and an equal literal; one clause and the content
    // constraint remain.
    let feeds = graph.edge(pair, labelled).expect("pair can feed labelled");
    assert!(!feeds.hypothetical && !feeds.bounded);
    assert_eq!(feeds.overlaps.len(), 1);
    let overlap = &feeds.overlaps[0];
    assert_eq!(overlap.pairs.len(), 3);
    assert_eq!(overlap.remaining.len(), 1);
    assert!(!overlap.unconditional(&view.rules[labelled]));
    let shared = overlap
        .bindings
        .iter()
        .find(|binding| binding.consumer.len() == 1 && !binding.literals.is_empty())
        .expect("the literal kind is a binding");
    assert!(shared.conditions.is_empty(), "{:?}", shared.conditions);
    // One consumer term (the pair) is reused by all three met clauses.
    let consumer = &view.rules[labelled];
    assert!(overlap.bindings.iter().any(|binding| {
        binding.consumer.iter().any(|&term| {
            overlap
                .pairs
                .iter()
                .filter(|(m, _)| consumer.clauses[*m].subject == term)
                .count()
                == 3
        })
    }));

    // Disjoint literals: no edge, and not undecided either.
    assert!(graph.edge(pair, other_kind).is_none());
    assert!(!graph
        .undecided
        .iter()
        .any(|u| u.from == pair && u.to == other_kind));

    // A literal meeting a constructed identity is only structurally compatible.
    let derived = graph.edge(pair, fixed).expect("pair can feed fixed");
    assert!(derived.overlaps[0]
        .bindings
        .iter()
        .any(|binding| binding.conditions.contains(&web::Condition::DerivedLiteral)));

    // Edges through the refused rule are structural only and form no cycle.
    for (from, to) in [(labelled, refused), (refused, pair), (refused, multi)] {
        let edge = graph.edge(from, to).expect("structural edge");
        assert!(edge.hypothetical, "{from} → {to}");
    }
    assert!(!graph.edge(pair, labelled).unwrap().hypothetical);
    assert_eq!(all.cycles.len(), 1, "{:?}", all.cycles);
    let cycle = &all.cycles[0];
    assert_eq!(cycle.members, vec![multi]);
    assert_eq!(cycle.walk, vec![multi]);
    assert!(!cycle.constructs && cycle.settles);
    assert_eq!(
        graph.edge(multi, multi).unwrap().overlaps[0]
            .remaining
            .len(),
        1
    );

    // The other reading is its own scope: it lost a body, so it is not complete,
    // and nothing from the first reading's selection is combined into it.
    assert_eq!(second.members, {
        let mut members = vec![pair, other_kind];
        members.sort();
        members
    });
    assert_eq!(second.missing, 1);
    assert!(!second.complete());
    assert_eq!(graph.edges_in(second).count(), 0);

    // Bounded evidence: a search stopped before any witness is undecided, never
    // an edge, never part of a cycle, and makes the scope incomplete.
    let bounded = web::web_with(
        &view,
        web::Limits {
            nodes: 1,
            ..Default::default()
        },
    );
    assert!(bounded.edges.is_empty(), "{:?}", bounded.edges);
    assert!(bounded
        .undecided
        .iter()
        .any(|u| u.from == pair && u.to == labelled && u.why == web::Unsearched::Bound));
    let all_bounded = reading_scope(&bounded, ENVIRONMENT_ANCHOR);
    assert!(!all_bounded.complete());
    assert!(all_bounded.cycles.is_empty());
    let spent = web::web_with(
        &view,
        web::Limits {
            total_nodes: 0,
            ..Default::default()
        },
    );
    assert!(spent.edges.is_empty());
    assert!(spent
        .undecided
        .iter()
        .all(|u| u.why == web::Unsearched::Budget));
    assert!(!spent.undecided.is_empty());
    let narrow = web::web_with(
        &view,
        web::Limits {
            rules: 3,
            ..Default::default()
        },
    );
    assert_eq!(narrow.analysed.len(), 3);
    assert!(narrow.scopes.iter().any(|scope| scope.beyond > 0));
    fixture.pile.close().unwrap();
}

#[test]
fn an_omitted_match_prevents_an_unconditional_overlap() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let signer = SigningKey::from_bytes(&[43; 32]);
    let mut pile = Pile::open_as(file.path(), signer.verifying_key()).unwrap();
    let root = pile
        .collection(
            "Partly decoded rule",
            CollectionPolicy::new(AdmissionPolicy::Open, AdmissionPolicy::Open),
        )
        .unwrap();
    let rule_id = genid().id;
    let input = genid().id;
    let (s, o) = (genid().id, genid().id);
    let mut predicates: Vec<_> = (0..=projection::MAX_COMBINATIONS)
        .map(|_| genid().id)
        .collect();
    predicates.sort();
    let mut program = entity! { ExclusiveId::force_ref(&rule_id) @
        rule::operation: rule::POSITIVE,
        rule::matches: input,
    };
    program += entity! { ExclusiveId::force_ref(&input) @
        rule::subject: s, rule::object: o, rule::predicate*: predicates.iter().copied(),
    };
    // One firing emits every displayed predicate, but never the omitted last one.
    for &predicate in &predicates[..projection::MAX_COMBINATIONS] {
        let output = genid().id;
        program += entity! { ExclusiveId::force_ref(&rule_id) @ rule::emits: output };
        program += entity! { ExclusiveId::force_ref(&output) @
            rule::subject: s, rule::predicate: predicate, rule::object: o,
        };
    }
    pile.commit(root, &signer, program).unwrap();
    let routes = CollectionRoutes::from([(schemas::rules::DEFAULT_SCOPE_ID, root.handle())]);
    let observation =
        pollster::block_on(WorkspaceObservation::observe(&mut pile, &signer, &routes)).unwrap();
    let context = observation.loaded_context();
    let view = projection::project(context.dataset(SourceKey::Rules).unwrap(), None);
    let index = rule_index(&view, rule_id);
    let consumer = &view.rules[index];
    assert!(!consumer.partial.is_empty());
    let graph = web::web(&view);
    let edge = graph.edge(index, index).expect("witnessed self-edge");
    let overlap = edge
        .overlaps
        .iter()
        .find(|o| o.remaining.is_empty())
        .unwrap();
    assert_eq!(overlap.pairs.len(), projection::MAX_COMBINATIONS);
    assert!(overlap.bindings.iter().all(|b| b.conditions.is_empty()));
    assert!(!overlap.unconditional(consumer));
    pile.close().unwrap();
}

#[test]
fn a_selected_revision_beyond_the_listing_bound_is_still_analysed() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let signer = SigningKey::from_bytes(&[37; 32]);
    let mut pile = Pile::open_as(file.path(), signer.verifying_key()).unwrap();
    let root = pile
        .collection(
            "Rules listing fixture",
            CollectionPolicy::new(AdmissionPolicy::Open, AdmissionPolicy::Open),
        )
        .unwrap();
    // More revisions than are listed; the selected one has the highest id, so
    // listing by id alone would drop it.
    let positive: Inline<GenId> = rule::POSITIVE.to_inline();
    let mut facts = TribleSet::new();
    for _ in 0..projection::MAX_RULES + 20 {
        facts.insert(&Trible::force(
            &genid().id,
            &rule::operation.id(),
            &positive,
        ));
    }
    let high = Id::new([0xff; 16]).unwrap();
    facts.insert(&Trible::force(&high, &rule::operation.id(), &positive));
    let mut program = Fragment::from(facts);
    program += entity! { _ @
        environment::reading_of: ENVIRONMENT_ANCHOR,
        environment::selects*: [high],
    };
    pile.commit(root, &signer, program).unwrap();
    let routes = CollectionRoutes::from([(schemas::rules::DEFAULT_SCOPE_ID, root.handle())]);
    let observation =
        pollster::block_on(WorkspaceObservation::observe(&mut pile, &signer, &routes)).unwrap();
    let context = observation.loaded_context();
    let view = projection::project(context.dataset(SourceKey::Rules).unwrap(), None);
    assert_eq!(view.revisions_total, projection::MAX_RULES + 21);
    assert_eq!(view.rules.len(), projection::MAX_RULES);
    assert_eq!(view.rules[0].revision, high);
    assert_eq!(view.selected_unlisted, 0);
    let graph = web::web(&view);
    assert_eq!(graph.scopes.len(), 1);
    assert_eq!(graph.scopes[0].members, vec![0]);
    assert!(graph.scopes[0].complete());
    pile.close().unwrap();
}

#[test]
fn a_diagram_arrow_is_picked_by_its_own_side() {
    use egui::pos2;
    use faculties::widgets::rules::diagram::{self, Mark};
    let (a, b) = (pos2(0.0, 0.0), pos2(100.0, 0.0));
    let marks = [
        diagram::link(a, b),
        diagram::link(b, a),
        diagram::self_loop(pos2(50.0, 60.0), pos2(50.0, 0.0)),
    ];
    assert!(matches!(marks[2], Mark::Loop { .. }));
    // A → B and B → A are drawn apart, each on its own side.
    assert_eq!(
        diagram::pick(&marks, pos2(50.0, diagram::SIDE), 2.0),
        Some(0)
    );
    assert_eq!(
        diagram::pick(&marks, pos2(50.0, -diagram::SIDE), 2.0),
        Some(1)
    );
    let Mark::Loop { centre } = marks[2] else {
        unreachable!()
    };
    assert_eq!(
        diagram::pick(&marks, centre + egui::vec2(diagram::LOOP, 0.0), 2.0),
        Some(2)
    );
    assert_eq!(diagram::pick(&marks, pos2(50.0, 30.0), 2.0), None);
    assert_eq!(diagram::ring(1, pos2(5.0, 5.0), 40.0), vec![pos2(5.0, 5.0)]);
    assert_eq!(diagram::ring(4, pos2(0.0, 0.0), 10.0).len(), 4);
}

#[test]
fn the_rule_web_is_explorable_and_says_what_its_evidence_cannot() {
    let mut fixture = WebFixture::new();
    let observation = fixture.observe();
    let before = std::fs::read(fixture.file.path()).unwrap();
    let host = FixtureHost {
        observation: Rc::new(RefCell::new(observation)),
        generation: Rc::new(Cell::new(1)),
    };
    let ctx = egui::Context::default();
    let nb_host = host.clone();
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
    let short = |id: Id| format!("{id:x}")[..8].to_owned();
    screen.wait("rule revisions");
    screen.click(&format!("Reading of {}", short(ENVIRONMENT_ANCHOR)));
    let all = screen.wait("R5 = pair");
    for expected in [
        "Current readings select different revisions",
        "refused by the engine:",
        "R1 = fixed · R2 = labelled · R3 = multi · R4 = other_kind · R5 = pair · R6 = refused",
        "pair can feed labelled: 1 maximal overlap",
        "pair can feed fixed: 1 maximal overlap",
        "refused can feed pair: 1 maximal overlap (structural only: a refused revision)",
        "Static feedback cycle: multi can feed itself",
        "it settles whenever its inputs are finite",
    ] {
        assert!(all.contains(expected), "missing {expected:?} in {all:?}");
    }
    // Disjoint literals draw no edge; the cycle through the refused rule is
    // not reported as feedback.
    assert!(!all.contains("pair can feed other_kind"), "{all:?}");
    assert!(!all.contains("Static feedback cycle among"), "{all:?}");
    for label in ["R1", "R2", "R3", "R4", "R5", "R6"] {
        assert!(screen.last.iter().any(|(text, _)| text == label), "{label}");
    }

    // Inspecting an edge shows its clauses, bindings and what remains.
    screen.click("pair can feed labelled: 1 maximal overlap");
    let edge = screen.wait("overlap 1: 3 of 4 matches clauses met");
    for expected in [
        "binds ",
        "still needs (",
        "@label",
        "sharing ",
        "still needs STRING_CONTAINS(",
    ] {
        assert!(edge.contains(expected), "missing {expected:?} in {edge:?}");
    }
    assert!(!edge.contains("no remaining conditions"), "{edge:?}");
    screen.click("pair can feed fixed: 1 maximal overlap");
    let derived = screen.wait("would still have to equal the identity pair constructs at runtime");
    assert!(
        !derived.contains("overlap 1: 3 of 4"),
        "one edge is inspected at a time"
    );
    screen.click("refused can feed pair: 1 maximal overlap (structural only: a refused revision)");
    screen
        .wait("Structural only: the engine refuses refused, so this edge cannot occur as stored.");

    // A mark in the diagram opens its rule.
    screen.click("R5");
    let opened = screen.wait("Stored conclusion-shaped facts");
    assert!(
        opened.contains(&format!("revision {:x}", fixture.rules[0])),
        "{opened:?}"
    );
    assert!(opened.contains("pair can feed labelled: 1 maximal overlap"));
    screen.click("← All rules");

    // The other reading: its own scope, with a lost body, so no absence is
    // claimed, and nothing from the first reading is combined into it.
    screen.wait("rule revisions");
    screen.click(&format!("Reading of {}", short(fixture.second)));
    let second = screen.wait("R2 = pair");
    for expected in [
        "1 selected revision(s) have no rule facts in this collection and are not analysed.",
        "No edge was witnessed. The evidence above is incomplete",
        "termination is not stated",
        "Selected but with no rule facts in this collection:",
    ] {
        assert!(
            second.contains(expected),
            "missing {expected:?} in {second:?}"
        );
    }
    for leaked in [
        "No analysed rule can feed another",
        "pair can feed labelled",
        "Static feedback cycle",
    ] {
        assert!(!second.contains(leaked), "{leaked:?} in {second:?}");
    }

    assert!(
        std::fs::read(fixture.file.path()).unwrap() == before,
        "exploring the web changed the pile"
    );
    fixture.pile.close().unwrap();
}
