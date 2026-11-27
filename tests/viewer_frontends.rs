use std::collections::BTreeSet;
use std::io::Cursor;
use std::time::Duration;

use faculties::mcp::{Faculty, InvalidArguments};
use faculties::out::{Out, Part};
use faculties::viewer::{self, CaptureOptions, CapturePage, Target, Viewer};

fn tiny_page() -> CapturePage {
    let mut encoded = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
        2,
        1,
        image::Rgba([10, 20, 30, 255]),
    ))
    .write_to(&mut encoded, image::ImageFormat::Png)
    .unwrap();
    CapturePage {
        card_index: 1,
        page_index: 0,
        page_count: 1,
        width: 2,
        height: 1,
        png: encoded.into_inner().into(),
    }
}

#[test]
fn target_inventory_and_original_dashboard_order_are_explicit() {
    assert_eq!(Target::ALL.len(), 13);
    assert_eq!(
        Target::ALL
            .into_iter()
            .map(Target::name)
            .collect::<BTreeSet<_>>()
            .len(),
        13
    );
    assert_eq!(
        Target::Dashboard.card_names(),
        &[
            "storage",
            "profiles",
            "colony",
            "rules",
            "timeline",
            "knowledge-health",
            "knowledge",
            "goals",
            "priority-game",
            "decisions",
            "mail",
            "events",
            "messages",
            "discord",
            "teams",
            "relations",
            "memory",
            "files",
            "diagnostics",
            "schemas",
        ]
    );
    for target in Target::ALL
        .into_iter()
        .filter(|target| *target != Target::Dashboard)
    {
        assert_eq!(target.card_names(), &["storage", target.name()]);
    }
    let adapter = viewer::mcp::Viewer::new("/not-opened.pile", None);
    assert_eq!(adapter.tools().len(), 1);
    let schema: serde_json::Value = serde_json::from_str(adapter.tools()[0].input_schema).unwrap();
    let targets: BTreeSet<_> = schema["properties"]["target"]["enum"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap())
        .collect();
    assert_eq!(targets, Target::ALL.into_iter().map(Target::name).collect());
    assert_eq!(schema["required"], serde_json::json!(["target"]));
}

#[test]
fn dashboard_capture_labels_follow_rendered_card_order() {
    use GORBIE::prelude::CardCtx;
    use faculties::storage::Storage;
    use faculties::viewer::{Host, HostAction, compose_hosted};
    use faculties::widgets::{SourceKey, StorageState, WidgetContext};
    use std::cell::RefCell;
    use std::rc::Rc;

    fn text(shape: &egui::epaint::Shape, output: &mut Vec<String>) {
        match shape {
            egui::epaint::Shape::Text(t) => output.push(t.galley.text().to_owned()),
            egui::epaint::Shape::Vec(shapes) => {
                for shape in shapes {
                    text(shape, output);
                }
            }
            _ => {}
        }
    }
    #[derive(Clone)]
    struct RecordingHost {
        storage: Rc<RefCell<StorageState>>,
        // One layout frame's output only, cleared when composing the next.
        cards: Rc<RefCell<Vec<Vec<String>>>>,
    }
    impl Host for RecordingHost {
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
            ctx.set_default_section_open(false);
            let painter = ctx.ui_mut().painter().clone();
            let mut before = 0;
            painter.for_each_shape(|_| before += 1);
            render(ctx, self.storage.borrow().loaded_context(), 0, &mut |_| {
                panic!("collapsed capture labels must not request an action")
            });
            let mut index = 0;
            let mut labels = Vec::new();
            painter.for_each_shape(|shape| {
                if index >= before {
                    text(&shape.shape, &mut labels);
                }
                index += 1;
            });
            self.cards.borrow_mut().push(labels);
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let pile = directory.path().join("capture-labels.pile");
    let key = directory.path().join("capture-labels.key");
    std::fs::File::create(&pile).unwrap();
    faculties::storage::initialize_signer(&pile, Some(&key)).unwrap();
    let mut state = StorageState::with_storage(
        Storage::new(pile, Some(key)),
        [SourceKey::Goals, SourceKey::Decisions, SourceKey::Schemas],
    );
    for source in [SourceKey::Goals, SourceKey::Decisions, SourceKey::Schemas] {
        assert!(state.context().dataset(source).is_some(), "{source:?}");
    }
    let cards = Rc::new(RefCell::new(Vec::new()));
    let host = RecordingHost {
        storage: Rc::new(RefCell::new(state)),
        cards: cards.clone(),
    };
    let ctx = egui::Context::default();
    let mut notebook = GORBIE::NotebookConfig::new("Capture labels")
        .without_editor()
        .embedded(&ctx, move |nb| {
            host.cards.borrow_mut().clear();
            // Hosted composition owns every subsequent card; storage/selector
            // occupies index zero, as in both actual dashboard hosts.
            nb.view(|ctx| {
                ctx.ui_mut().label("Storage");
            });
            compose_hosted(nb, Target::Dashboard, host.clone());
        });
    ctx.style_mut(|style| style.animation_time = 0.0);
    let _ = ctx.run_ui(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1100.0, 1600.0),
            )),
            ..Default::default()
        },
        |ui| notebook.show(ui, None),
    );

    let cards = cards.borrow();
    assert_eq!(cards.len() + 1, Target::Dashboard.card_names().len());
    for (title, name) in [
        ("Goals", "goals"),
        ("Priority game", "priority-game"),
        ("Decisions", "decisions"),
        ("Schemas", "schemas"),
    ] {
        let matches: Vec<_> = cards
            .iter()
            .enumerate()
            .filter(|(_, labels)| labels.iter().any(|label| label == title))
            .collect();
        assert_eq!(matches.len(), 1, "{title}: {cards:?}");
        let mut page = tiny_page();
        page.card_index = matches[0].0 + 1;
        let mut parts = Vec::new();
        page.emit(
            Target::Dashboard,
            &mut Out::new(&mut |part| {
                parts.push(part);
                Ok(())
            }),
        )
        .unwrap();
        let Part::Text { text } = &parts[0] else {
            panic!("missing metadata")
        };
        let emitted: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(emitted["card"], name, "{title}: {emitted}");
    }
    // This is real CPU card composition/layout plus metadata emission. The
    // tiny PNG is synthetic; no native screenshot/GPU rendering is claimed.
}

#[test]
fn all_native_bounds_fail_before_gpu_or_storage_acquisition() {
    let directory = tempfile::tempdir().unwrap();
    let viewer = Viewer::new(
        directory.path().join("absent.pile"),
        Some(directory.path().join("absent.key")),
    );
    let invalid = [
        CaptureOptions {
            scale: 0.0,
            ..Default::default()
        },
        CaptureOptions {
            scale: f32::NAN,
            ..Default::default()
        },
        CaptureOptions {
            scale: f32::INFINITY,
            ..Default::default()
        },
        CaptureOptions {
            scale: 4.1,
            ..Default::default()
        },
        CaptureOptions {
            settle_timeout: Duration::from_millis(5001),
            ..Default::default()
        },
        CaptureOptions {
            max_images: 0,
            ..Default::default()
        },
        CaptureOptions {
            max_images: 257,
            ..Default::default()
        },
        CaptureOptions {
            max_bytes: 0,
            ..Default::default()
        },
        CaptureOptions {
            max_bytes: usize::MAX,
            ..Default::default()
        },
    ];
    for options in invalid {
        assert!(viewer
            .capture_with(Target::Files, &options, |_| panic!(
                "invalid capture emitted"
            ))
            .is_err());
    }
    CaptureOptions {
        settle_timeout: Duration::ZERO,
        ..Default::default()
    }
    .validate()
    .unwrap();
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
}

#[test]
fn resident_pages_emit_metadata_then_exact_png_not_a_file_resource() {
    let page = tiny_page();
    let mut output = Vec::new();
    page.emit(
        Target::Files,
        &mut Out::new(&mut |part| {
            output.push(part);
            Ok(())
        }),
    )
    .unwrap();
    assert_eq!(output.len(), 2);
    let Part::Text { text } = &output[0] else {
        panic!("metadata precedes image")
    };
    let metadata: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(metadata["target"], "files");
    assert_eq!(metadata["card"], "files");
    assert_eq!(metadata["card_index"], 1);
    assert_eq!(metadata["page_index"], 0);
    assert_eq!(metadata["page_count"], 1);
    assert_eq!(metadata["width"], 2);
    let Part::Image { bytes, mime_type } = &output[1] else {
        panic!("resident image, not path/blob")
    };
    assert_eq!(mime_type, "image/png");
    assert_eq!(bytes, &page.png);
    let decoded = image::load_from_memory(bytes.as_ref())
        .unwrap()
        .into_rgba8();
    assert_eq!(decoded.dimensions(), (2, 1));
    assert_eq!(decoded.get_pixel(1, 0).0, [10, 20, 30, 255]);
}

#[test]
fn output_failure_retains_exact_prefix_and_never_repeats_it() {
    let page = tiny_page();
    for fail_at in [0, 1] {
        let mut calls = 0;
        let mut accepted = Vec::new();
        let error = page
            .emit(
                Target::Files,
                &mut Out::new(&mut |part| {
                    let index = calls;
                    calls += 1;
                    if index == fail_at {
                        anyhow::bail!("receiver closed");
                    }
                    accepted.push(part);
                    Ok(())
                }),
            )
            .unwrap_err();
        assert!(error.to_string().contains("receiver closed"));
        assert_eq!(calls, fail_at + 1);
        assert_eq!(accepted.len(), fail_at);
        assert!(accepted
            .iter()
            .all(|part| matches!(part, Part::Text { .. })));
    }
}

#[test]
fn mcp_is_strict_finite_and_cannot_supply_host_paths_or_harness_flags() {
    let directory = tempfile::tempdir().unwrap();
    let adapter = viewer::mcp::Viewer::new(directory.path().join("absent.pile"), None);
    for input in [
        "[]",
        "{}",
        r#"{"target":"files","target":"memory"}"#,
        r#"{"target":"status"}"#,
        r#"{"target":"@/host/path"}"#,
        r#"{"target":"files","pile":"/host/path"}"#,
        r#"{"target":"files","key":"/host/key"}"#,
        r#"{"target":"files","out_dir":"/host/path"}"#,
        r#"{"target":"files","export":true}"#,
        r#"{"target":"files","persona":"ambient"}"#,
        r#"{"target":"files","scale":0}"#,
        r#"{"target":"files","scale":1e99}"#,
        r#"{"target":"files","settle_ms":5001}"#,
        r#"{"target":"files","max_images":0}"#,
        r#"{"target":"files","max_bytes":0}"#,
    ] {
        let error = adapter
            .call(
                "viewer_capture",
                input.to_owned().into(),
                &mut Out::new(&mut |_| panic!("invalid capture emitted")),
            )
            .unwrap_err();
        assert!(
            error.downcast_ref::<InvalidArguments>().is_some(),
            "{input}: {error:#}"
        );
    }
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
}

#[test]
#[cfg(not(feature = "widgets"))]
fn unavailable_renderer_fails_honestly_without_touching_launcher_paths() {
    let directory = tempfile::tempdir().unwrap();
    let adapter = viewer::mcp::Viewer::new(directory.path().join("absent.pile"), None);
    for target in Target::ALL {
        let error = adapter
            .call(
                "viewer_capture",
                format!("{{\"target\":\"{}\"}}", target.name()).into(),
                &mut Out::new(&mut |_| panic!("renderer unavailable")),
            )
            .unwrap_err();
        assert!(error.to_string().contains("native `widgets` feature"));
        assert!(error.downcast_ref::<InvalidArguments>().is_none());
    }
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
}

#[test]
fn cli_information_is_resolved_without_the_notebook() {
    for flag in ["--help", "-h"] {
        let text = viewer::cli::information("files-capture", &[flag.into()]).unwrap();
        assert!(text.contains("Usage: files-capture"));
        for name in [
            "--pile",
            "--headless",
            "--out-dir",
            "--scale",
            "--headless-wait-ms",
            "--export",
            "--export-dir",
        ] {
            assert!(text.contains(name));
        }
    }
    assert!(viewer::cli::information("viewer", &["--version".into()])
        .unwrap()
        .starts_with("viewer "));
    assert!(viewer::cli::information("viewer", &["--headless".into()]).is_none());
}

#[test]
#[cfg(feature = "widgets")]
fn thin_wrapper_help_and_version_do_not_open_a_window_or_gpu() {
    for binary in [
        env!("CARGO_BIN_EXE_viewer"),
        env!("CARGO_BIN_EXE_files-capture"),
    ] {
        for flag in ["--help", "--version"] {
            let output = std::process::Command::new(binary)
                .arg(flag)
                .env("PILE", "/deliberately-absent.pile")
                .env_remove("DISPLAY")
                .env_remove("WAYLAND_DISPLAY")
                .env_remove("DRIVE_ENDPOINT")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(!output.stdout.is_empty());
        }
    }
}
