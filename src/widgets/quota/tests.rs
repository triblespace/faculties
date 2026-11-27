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
