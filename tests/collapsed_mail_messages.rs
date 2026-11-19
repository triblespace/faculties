//! Exercise the dashboard's real section bodies without a window or a graphics device.
#![cfg(feature = "widgets")]

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use ed25519_dalek::SigningKey;
use faculties::relations::{person_fragment, ProfileInput};
use faculties::schemas::{mail, message, relations};
use faculties::storage::CollectionRoutes;
use faculties::widgets::storage::WorkspaceObservation;
use faculties::widgets::{DatasetView, MailViewer, MessagesPanel, SourceKey};
use triblespace::core::collection::observed_store::DependencyTracker;
use triblespace::core::collection::{AdmissionPolicy, CollectionPolicy, CollectionStoreExt};
use triblespace::core::repo::pile::Pile;
use triblespace::prelude::*;
use GORBIE::prelude::CardCtx;

#[test]
fn collapsed_mail_defers_projection_until_reopened() {
    check_section(
        "Mail",
        SourceKey::Mail,
        mail::DEFAULT_SCOPE_ID,
        MailViewer::default,
        MailViewer::render,
    );
}

#[test]
fn collapsed_messages_defers_projection_until_reopened() {
    check_section(
        "Messages",
        SourceKey::Messages,
        message::DEFAULT_SCOPE_ID,
        MessagesPanel::default,
        MessagesPanel::render,
    );
}

fn check_section<T: Send + Sync + 'static>(
    title: &'static str,
    source: SourceKey,
    scope: Id,
    init: fn() -> T,
    render: fn(&mut T, &mut CardCtx<'_>, DatasetView<'_>, Option<DatasetView<'_>>),
) {
    let file = tempfile::NamedTempFile::new().unwrap();
    let signer = SigningKey::from_bytes(&[17; 32]);
    let mut pile = Pile::open_as(file.path(), signer.verifying_key()).unwrap();
    let policy = || CollectionPolicy::new(AdmissionPolicy::Open, AdmissionPolicy::Open);
    let dataset = pile.collection(title, policy()).unwrap();
    let people = pile.collection("People", policy()).unwrap();
    let routes = CollectionRoutes::from([
        (scope, dataset.handle()),
        (relations::DEFAULT_SCOPE_ID, people.handle()),
    ]);
    let label = "Synthetic contact";
    let (person, _, _) = person_fragment(
        genid().id,
        ProfileInput {
            label: label.to_owned(),
            ..Default::default()
        },
    )
    .unwrap();
    pile.commit(people, &signer, person).unwrap();
    let label_blob: Blob<blobencodings::UTF8String> = label.to_blob();
    let label_hash = label_blob.get_handle().raw;

    let tracker = DependencyTracker::default();
    let mut observe = |serial| {
        pollster::block_on(WorkspaceObservation::observe_tracked(
            &mut pile,
            &signer,
            &routes,
            tracker.clone(),
        ))
        .unwrap()
        .with_presentation_revision(serial)
    };
    let observation = Rc::new(RefCell::new(observe(1)));
    let expanded = Rc::new(Cell::new(false));
    let ctx = egui::Context::default();
    let mut notebook = GORBIE::NotebookConfig::new("Synthetic dashboard")
        .without_editor()
        .embedded(&ctx, {
            let observation = observation.clone();
            let expanded = expanded.clone();
            move |notebook| {
                let observation = observation.clone();
                let expanded = expanded.clone();
                notebook.state(title, init, move |card, panel| {
                    let ui = card.ui_mut();
                    let id = ui.make_persistent_id(title);
                    ui.ctx()
                        .data_mut(|data| data.insert_persisted(id, expanded.get()));
                    let observation = observation.borrow();
                    let data = observation.loaded_context();
                    render(
                        panel,
                        card,
                        data.dataset(source).unwrap(),
                        data.dataset(SourceKey::Relations),
                    );
                });
            }
        });
    let mut check = |open, expect_label_read, reason| {
        expanded.set(open);
        tracker.lock().unwrap().blobs = Default::default();
        let _ = ctx.run_ui(Default::default(), |ui| notebook.show(ui, None));
        let was_read = tracker
            .lock()
            .unwrap()
            .blobs
            .iter()
            .any(|hash| hash.raw == label_hash);
        assert_eq!(was_read, expect_label_read, "{title}: {reason}");
    };
    check(false, false, "first closed paint");
    check(true, true, "first open reads current people");
    check(true, false, "same observation reuses the projection");
    *observation.borrow_mut() = observe(2);
    check(false, false, "replacement observation while closed");
    check(true, true, "reopen reads replacement observation");
    *observation.borrow_mut() = observe(3);
    check(true, true, "an open section refreshes live");
    pile.close().unwrap();
}
