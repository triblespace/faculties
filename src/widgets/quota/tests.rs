use std::rc::Rc;
use std::time::{Duration, Instant};

use ed25519_dalek::SigningKey;
use egui::epaint::Shape;
use triblespace::core::collection::{
    AdmissionPolicy, Collection, CollectionPolicy, CollectionStoreExt,
};
use triblespace::core::repo::pile::Pile;
use triblespace::prelude::*;

use super::*;
use crate::quota::{observation_fragment, Observation};
use crate::schemas::quota::{quota, DEFAULT_SCOPE_ID, PROVIDER_CLAUDE};
use crate::storage::CollectionRoutes;
use crate::viewer::{compose_hosted, Host, HostAction, Target};
use crate::widgets::storage::WorkspaceObservation;

#[test]
fn bounded_projection_preserves_account_window_bucket_and_equal_time_readings() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let signer = SigningKey::from_bytes(&[42; 32]);
    let mut pile = Pile::open_as(file.path(), signer.verifying_key()).unwrap();
    let collection = pile.collection("quota", open()).unwrap();
    let now = 1_790_000_000;
    let mut primary = reading("alpha-digest", now - 30_000, 30.0);
    primary.resets_at = now - 20_000;
    let mut over = reading("alpha-digest", now - 60, 104.5);
    over.resets_at = now + 600;
    let mut same_time = over.clone();
    same_time.used_percent = 71.0;
    same_time.host = "peer".into();
    let mut other_window = over.clone();
    other_window.window_secs = 7 * DAY as u64;
    let mut bucketed = over.clone();
    bucketed.bucket = Some("review".into());
    for reading in [
        primary,
        over,
        same_time,
        other_window,
        bucketed,
        reading("beta-digest", now - 60, 18.0),
        reading("future-digest", now + 60, 38.0),
    ] {
        publish(&mut pile, &signer, collection, reading);
    }
    let (mut fragment, event) =
        observation_fragment(&reading("alpha-digest", now - 121, 44.0)).unwrap();
    let unavailable: Blob<blobencodings::UTF8String> = "unpublished bucket label".to_blob();
    fragment += entity! {
        ExclusiveId::force_ref(&event) @ quota::bucket: unavailable.get_handle(),
    };
    pile.commit(collection, &signer, fragment).unwrap();
    let observed = observe(&mut pile, &signer, Some(collection));
    let view = observed.loaded_context().dataset(SourceKey::Quota).unwrap();
    let now = unix_to_tai_ns(now);
    let Projection::Ready(history) = project(view, now, DAY) else {
        panic!("the current observation should be readable");
    };
    assert_eq!(history.allowances.len(), 5);
    assert!(history
        .allowances
        .iter()
        .all(|group| !group.label.contains("-digest")));
    assert_eq!(
        history
            .allowances
            .iter()
            .filter(|group| group.key.bucket.is_some())
            .count(),
        2
    );
    let unavailable = history
        .allowances
        .iter()
        .find(|allowance| allowance.label.contains("label unavailable"))
        .unwrap();
    assert_eq!(unavailable.readings[0].used, [44.0]);
    assert_eq!(
        history
            .allowances
            .iter()
            .filter(|group| group.key.window == i128::from(7 * DAY) * SECOND)
            .count(),
        1
    );
    let main = history
        .allowances
        .iter()
        .find(|group| group.readings.len() == 3)
        .unwrap();
    let rows = &main.readings;
    assert!(rows[0].stale(now));
    assert!(rows[0].status(now).contains("reset since reading"));
    assert!(!rows[0].status(now).contains("resets in"));
    assert!(rows[1].at - rows[0].at > 7 * 3_600 * SECOND);
    assert_eq!(rows[1].at, rows[2].at);
    assert_ne!(rows[1].resets, rows[0].resets);
    let over = rows.iter().find(|reading| reading.used == [104.5]).unwrap();
    assert_eq!(over.amount(), "0% remaining");
    assert!(!over.stale(now));
    assert!(over.status(now).contains("resets in 10m"));
    let Projection::Ready(short) = project(view, now, 3_600) else {
        panic!("the shorter window should be readable");
    };
    assert_eq!(
        short
            .allowances
            .iter()
            .find(|group| group.key == main.key)
            .unwrap()
            .readings
            .len(),
        2
    );
    drop(observed);
    pile.close().unwrap();
}

#[test]
fn routed_dashboard_opens_quota_and_paints_recorded_points() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let signer = SigningKey::from_bytes(&[48; 32]);
    let mut pile = Pile::open_as(file.path(), signer.verifying_key()).unwrap();
    let now = tai_ns_to_unix(crate::clock::tai_nanoseconds_now().unwrap());
    let alternate = pile.collection("quota", open()).unwrap();
    publish(
        &mut pile,
        &signer,
        alternate,
        reading("alternate-digest", now - 240, 22.0),
    );

    let selected = pile
        .collection(
            "quota",
            CollectionPolicy::new(
                AdmissionPolicy::Open,
                AdmissionPolicy::direct(signer.verifying_key()),
            ),
        )
        .unwrap();
    publish(
        &mut pile,
        &signer,
        selected,
        reading("selected-digest", now - 3_500, 25.0),
    );
    publish(
        &mut pile,
        &signer,
        selected,
        reading("selected-digest", now - 240, 104.5),
    );
    let observed = observe(&mut pile, &signer, Some(selected));
    let exact = observed.loaded_context().dataset(SourceKey::Quota).unwrap();
    let Projection::Ready(history) = project(exact, unix_to_tai_ns(now), DAY) else {
        panic!("the selected route should supply the projection");
    };
    assert_eq!(
        history.allowances.len(),
        1,
        "another quota root must not contribute"
    );
    assert_eq!(history.allowances[0].readings.len(), 2);

    let frame = frame_containing(
        Target::Dashboard,
        observed,
        "104.5% used · 0% remaining · 4.5% over",
    );
    assert!(frame
        .text
        .iter()
        .any(|text| text.contains("Claude") && text.contains("Account")));
    assert!(frame
        .text
        .iter()
        .any(|text| text.contains("Blank periods have no readings")));
    assert!(!frame
        .text
        .iter()
        .any(|text| text.contains("selected-digest") || text.contains("alternate-digest")));
    assert_eq!(
        frame.dots.len(),
        2,
        "only the two recorded readings should be plotted"
    );
    assert!(frame.dots[0].x < frame.dots[1].x);
    assert!(
        frame.dots[0].y < frame.dots[1].y,
        "zero remaining should be below 75%"
    );

    let missing = observe(&mut pile, &signer, None);
    assert!(missing.loaded_context().missing_route(SourceKey::Quota));
    let frame = frame_containing(
        Target::Dashboard,
        missing,
        "Quota route missing from this workspace.",
    );
    assert!(frame.dots.is_empty());

    let denied = pile
        .collection(
            "quota",
            CollectionPolicy::new(
                AdmissionPolicy::direct(SigningKey::from_bytes(&[47; 32]).verifying_key()),
                AdmissionPolicy::Open,
            ),
        )
        .unwrap();
    let unavailable = observe(&mut pile, &signer, Some(denied));
    assert!(!unavailable.loaded_context().missing_route(SourceKey::Quota));
    let frame = frame_containing(
        Target::Dashboard,
        unavailable,
        "Quota collection unavailable.",
    );
    assert!(frame.dots.is_empty());

    let empty = pile.collection("empty-quota-fixture", open()).unwrap();
    let frame = frame_containing(
        Target::Quota,
        observe(&mut pile, &signer, Some(empty)),
        "No quota observations in this range.",
    );
    assert!(frame.dots.is_empty());
    pile.close().unwrap();
}

#[test]
fn explicitly_refuses_a_preview_with_more_values_than_it_can_safely_draw() {
    use triblespace::core::metadata;

    let file = tempfile::NamedTempFile::new().unwrap();
    let signer = SigningKey::from_bytes(&[50; 32]);
    let mut pile = Pile::open_as(file.path(), signer.verifying_key()).unwrap();
    let collection = pile.collection("quota", open()).unwrap();
    let now = 1_790_000_000;
    let (mut fragment, event) =
        observation_fragment(&reading("alpha-digest", now - 1, 10.0)).unwrap();
    let additional: Vec<_> = (0..=MAX_VALUES_PER_READING)
        .map(|value| value as f64)
        .collect();
    fragment += entity! {
        ExclusiveId::force_ref(&event) @
        quota::used_percent*: additional,
        metadata::tag: &crate::schemas::quota::KIND_QUOTA_OBSERVATION,
    };
    pile.commit(collection, &signer, fragment).unwrap();
    let observed = observe(&mut pile, &signer, Some(collection));
    let view = observed.loaded_context().dataset(SourceKey::Quota).unwrap();
    assert!(matches!(
        project(view, unix_to_tai_ns(now), DAY),
        Projection::TooManyReadings
    ));
    drop(observed);
    pile.close().unwrap();
}

#[test]
fn long_labels_stay_bounded_without_merging_native_allowances() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let signer = SigningKey::from_bytes(&[51; 32]);
    let mut pile = Pile::open_as(file.path(), signer.verifying_key()).unwrap();
    let collection = pile.collection("quota", open()).unwrap();
    let now = 1_790_000_000;
    let prefix = "é".repeat(20_000);
    for (suffix, used) in [("first", 12.0), ("second", 34.0)] {
        let mut input = reading("same-account", now - 1, used);
        input.bucket = Some(format!("{prefix}{suffix}"));
        publish(&mut pile, &signer, collection, input);
    }
    let unknown = ufoid();
    pile.commit(
        collection,
        &signer,
        entity! {
            &unknown @ metadata::name: format!("{prefix}provider"),
        },
    )
    .unwrap();
    let short = pile.put::<blobencodings::UTF8String, _>("café").unwrap();
    let observed = observe(&mut pile, &signer, Some(collection));
    let view = observed.loaded_context().dataset(SourceKey::Quota).unwrap();
    let Projection::Ready(history) = project(view, unix_to_tai_ns(now), DAY) else {
        panic!("long display labels must not reject otherwise usable readings");
    };
    assert_eq!(history.allowances.len(), 2);
    let a = &history.allowances[0];
    let b = &history.allowances[1];
    assert_ne!(a.key.bucket, b.key.bucket);
    assert_ne!(a.label, b.label);
    let mut used = vec![a.readings[0].used[0], b.readings[0].used[0]];
    used.sort_by(f64::total_cmp);
    assert_eq!(used, [12.0, 34.0]);
    for allowance in &history.allowances {
        assert!(allowance.label.contains("long label ["));
        assert!(allowance.label.len() < 128);
        assert!(!allowance.label.contains('é'));
    }
    let provider_label = allowance_label(
        view,
        GroupKey {
            provider: unknown.id,
            ..a.key
        },
    );
    assert!(provider_label.starts_with("long label ["));
    assert!(provider_label.len() < 128);
    assert!(!provider_label.contains('é'));
    // A short multibyte name remains complete; neither byte boundaries nor
    // a Unicode label require replacing the actual grouping identity.
    assert_eq!(display_text(view.reader, short).as_deref(), Some("café"));
    drop(observed);
    pile.close().unwrap();
}

#[test]
fn earlier_reading_click_changes_the_visible_native_observation() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let signer = SigningKey::from_bytes(&[52; 32]);
    let mut pile = Pile::open_as(file.path(), signer.verifying_key()).unwrap();
    let collection = pile.collection("quota", open()).unwrap();
    let now = 1_790_000_000;
    publish(
        &mut pile,
        &signer,
        collection,
        reading("account", now - 600, 20.0),
    );
    publish(
        &mut pile,
        &signer,
        collection,
        reading("account", now - 60, 80.0),
    );
    let observed = observe(&mut pile, &signer, Some(collection));
    let view = observed.loaded_context().dataset(SourceKey::Quota).unwrap();
    let now = unix_to_tai_ns(now);
    let Projection::Ready(history) = project(view, now, DAY) else {
        panic!("history")
    };
    let ctx = egui::Context::default();
    let (mut selected, mut inspected) = (None, None);
    let (mut allowance_page, mut reading_page) = (0, 0);
    let mut render = |events| {
        ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(900.0, 900.0))),
                events,
                ..Default::default()
            },
            |ui| {
                draw_history(
                    ui,
                    &history,
                    now,
                    &mut selected,
                    &mut allowance_page,
                    &mut inspected,
                    &mut reading_page,
                )
            },
        )
    };
    let first = render(vec![]);
    fn text_position(shape: &Shape, wanted: &str) -> Option<Pos2> {
        match shape {
            Shape::Text(text) if text.galley.text() == wanted => {
                Some(text.pos + text.galley.size() * 0.5)
            }
            Shape::Vec(shapes) => shapes.iter().find_map(|s| text_position(s, wanted)),
            _ => None,
        }
    }
    let button = first
        .shapes
        .iter()
        .find_map(|s| text_position(&s.shape, "Earlier reading"))
        .expect("actual Earlier reading button");
    let mut before = Frame::default();
    for shape in &first.shapes {
        before.visit(&shape.shape);
    }
    assert!(before.text.iter().any(|s| s.contains("80% used")));
    for pressed in [true, false] {
        let output = render(vec![
            egui::Event::PointerMoved(button),
            egui::Event::PointerButton {
                pos: button,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::default(),
            },
        ]);
        if !pressed {
            let mut after = Frame::default();
            for shape in &output.shapes {
                after.visit(&shape.shape);
            }
            assert!(after.text.iter().any(|s| s.contains("20% used")));
            assert!(!after.text.iter().any(|s| s.contains("80% used")));
        }
    }
    drop(render);
    assert_eq!(inspected, Some(history.allowances[0].readings[0].at));
    drop(observed);
    pile.close().unwrap();
}

fn reading(account: &str, observed_at: i64, used_percent: f64) -> Observation {
    Observation {
        provider: PROVIDER_CLAUDE,
        account: account.into(),
        window_secs: 5 * 3_600,
        used_percent,
        resets_at: observed_at + 3_600,
        observed_at,
        host: "fixture".into(),
        bucket: None,
    }
}

fn open() -> CollectionPolicy {
    CollectionPolicy::new(AdmissionPolicy::Open, AdmissionPolicy::Open)
}

fn publish(
    pile: &mut Pile,
    signer: &SigningKey,
    collection: Collection<blobencodings::SimpleArchive>,
    reading: Observation,
) {
    let (fragment, _) = observation_fragment(&reading).unwrap();
    pile.commit(collection, signer, fragment).unwrap();
}

fn observe(
    pile: &mut Pile,
    signer: &SigningKey,
    collection: Option<Collection<blobencodings::SimpleArchive>>,
) -> WorkspaceObservation {
    let routes = collection
        .into_iter()
        .map(|collection| (DEFAULT_SCOPE_ID, collection.handle()))
        .collect::<CollectionRoutes>();
    pollster::block_on(WorkspaceObservation::observe(pile, signer, &routes)).unwrap()
}

#[derive(Clone)]
struct DataHost(Rc<WorkspaceObservation>);

impl Host for DataHost {
    fn with_context(
        &self,
        ctx: &mut CardCtx<'_>,
        render: impl FnOnce(
            &mut CardCtx<'_>,
            WidgetContext<'_>,
            u64,
            &mut dyn FnMut(HostAction) -> Result<(), String>,
        ),
    ) {
        ctx.set_default_section_open(true);
        render(ctx, self.0.loaded_context(), 0, &mut |_| {
            panic!("quota is read-only")
        });
    }
}

#[derive(Default)]
struct Frame {
    text: Vec<String>,
    dots: Vec<Pos2>,
}

impl Frame {
    fn visit(&mut self, shape: &Shape) {
        match shape {
            Shape::Text(text) => self.text.push(text.galley.text().to_owned()),
            Shape::Circle(circle)
                if circle.radius == 3.0 && circle.fill == GORBIE::themes::button_light_on() =>
            {
                self.dots.push(circle.center);
            }
            Shape::Vec(shapes) => shapes.iter().for_each(|shape| self.visit(shape)),
            _ => {}
        }
    }
}

fn frame_containing(target: Target, observation: WorkspaceObservation, expected: &str) -> Frame {
    let ctx = egui::Context::default();
    let host = DataHost(Rc::new(observation));
    let mut notebook = GORBIE::NotebookConfig::new("Data quota control")
        .without_editor()
        .embedded(&ctx, move |nb| {
            nb.view(|ctx| {
                ctx.label("Workspace selector");
            });
            compose_hosted(nb, target, host.clone());
        });
    ctx.style_mut(|style| style.animation_time = 0.0);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(
                    Pos2::ZERO,
                    egui::vec2(1_200.0, 1_800.0),
                )),
                ..Default::default()
            },
            |ui| notebook.show(ui, None),
        );
        let mut frame = Frame::default();
        for shape in output.shapes {
            frame.visit(&shape.shape);
        }
        if frame.text.iter().any(|text| text.contains(expected)) {
            return frame;
        }
        assert!(
            Instant::now() < deadline,
            "missing {expected:?} from {:?}",
            frame.text
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A commit whose payload is not here: the routed Quota view still shows
/// the resident readings and names the missing foundation, and a range that
/// holds nothing resident is not presented as an empty history.
#[test]
fn a_stranded_foundation_shows_what_is_here_and_never_an_empty_history() {
    use triblespace::core::collection::{
        empty_metadata_handle, CollectionCommit, CollectionRecord, CollectionStore,
    };
    use triblespace::prelude::inlineencodings::Handle;

    fn strand(
        pile: &mut Pile,
        signer: &SigningKey,
        collection: Collection<blobencodings::SimpleArchive>,
    ) -> String {
        let withheld: Blob<blobencodings::SimpleArchive> =
            observation_fragment(&reading("withheld-digest", 1_790_000_000, 12.0))
                .unwrap()
                .0
                .into_facts()
                .to_blob();
        pile.insert(CollectionRecord::Commit(CollectionCommit::sign(
            signer,
            collection.handle(),
            Handle::<blobencodings::SimpleArchive>::to_hash(withheld.get_handle()),
            empty_metadata_handle(),
        )))
        .unwrap();
        hex::encode(withheld.get_handle().raw)
    }

    let file = tempfile::NamedTempFile::new().unwrap();
    let signer = SigningKey::from_bytes(&[51; 32]);
    let mut pile = Pile::open_as(file.path(), signer.verifying_key()).unwrap();
    let now = tai_ns_to_unix(crate::clock::tai_nanoseconds_now().unwrap());
    let collection = pile.collection("quota", open()).unwrap();
    publish(
        &mut pile,
        &signer,
        collection,
        reading("resident-digest", now - 240, 33.0),
    );
    let away = strand(&mut pile, &signer, collection);
    let observed = observe(&mut pile, &signer, Some(collection));
    let context = observed.loaded_context();
    assert!(!context.missing_route(SourceKey::Quota));
    let view = context.dataset(SourceKey::Quota).unwrap();
    assert!(
        view.notes.iter().any(|note| note.contains(&away)),
        "{:?}",
        view.notes
    );
    assert!(observed.partial_notes().any(|note| note.contains(&away)));
    let Projection::Ready(history) = project(view, unix_to_tai_ns(now), DAY) else {
        panic!("the resident reading should be readable");
    };
    assert_eq!(history.allowances.len(), 1);
    let frame = frame_containing(Target::Quota, observed, "Showing what is here");
    assert!(frame.text.iter().any(|text| text.contains(&away)));

    let lonely = pile.collection("stranded-quota-fixture", open()).unwrap();
    let away = strand(&mut pile, &signer, lonely);
    let observed = observe(&mut pile, &signer, Some(lonely));
    let frame = frame_containing(
        Target::Quota,
        observed,
        "No quota observations here in this range; some could not be loaded.",
    );
    assert!(frame.text.iter().any(|text| text.contains(&away)));
    assert!(!frame
        .text
        .iter()
        .any(|text| text == "No quota observations in this range."));
    pile.close().unwrap();
}
