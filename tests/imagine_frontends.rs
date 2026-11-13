use faculties::imagine::{self, Flux, FluxOptions, GeneratedImage, ModelSources, Variant};
use faculties::mcp::{Faculty, InvalidArguments};
use faculties::out::Out;
use std::time::Duration;

fn options() -> FluxOptions {
    FluxOptions {
        prompt: "@/literal-not-a-path".into(),
        variant: Variant::Klein,
        steps: None,
        seed: 0,
        width: 1024,
        height: 1024,
        guidance: 4.0,
    }
}
fn sources(root: &std::path::Path) -> ModelSources {
    ModelSources {
        hub: Some(root.join("hub")),
        weights_root: root.join("weights"),
        weights_override: None,
    }
}

#[test]
fn native_requests_are_literal_and_validated_before_model_acquisition() {
    let directory = tempfile::tempdir().unwrap();
    let generator = Flux::new(sources(directory.path()));
    let mut request = options();
    request.validate().unwrap();
    assert_eq!(request.prompt, "@/literal-not-a-path");
    assert_eq!(Variant::Klein.default_steps(), 8);
    assert_eq!(Variant::Dev.default_steps(), 28);
    for width in [0, 15, 17, 4097, usize::MAX] {
        request.width = width;
        let error = generator.generate(&request).unwrap_err();
        assert!(error.to_string().contains("width"));
    }
    request = options();
    for steps in [0, 201] {
        request.steps = Some(steps);
        assert!(generator
            .generate(&request)
            .unwrap_err()
            .to_string()
            .contains("steps"));
    }
    request = options();
    request.guidance = f32::NAN;
    assert!(request.validate().is_err());
    request.guidance = -1.0;
    assert!(request.validate().is_err());
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
}

#[test]
fn generated_png_is_resident_and_host_saving_is_explicit() {
    let image = image::RgbImage::from_fn(2, 1, |x, _| {
        image::Rgb(if x == 0 { [0, 0, 0] } else { [255, 255, 255] })
    });
    let generated = GeneratedImage::from_rgb(image, Duration::from_millis(50)).unwrap();
    assert_eq!((generated.width, generated.height), (2, 1));
    assert!((generated.luminance_mean - 127.5).abs() < 1e-8);
    assert!((generated.luminance_stddev - 127.5).abs() < 1e-8);
    assert!(!generated.diagnostic().contains("WARNING"));
    let decoded = image::load_from_memory(generated.png.as_ref())
        .unwrap()
        .into_rgb8();
    assert_eq!(decoded.get_pixel(1, 0).0, [255, 255, 255]);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("new/generated.png");
    assert!(!path.exists());
    generated.save(&path).unwrap();
    assert_eq!(std::fs::read(path).unwrap(), generated.png.as_ref());
    let blank = GeneratedImage::from_rgb(image::RgbImage::new(1, 1), Duration::ZERO).unwrap();
    assert!(blank.diagnostic().contains("WARNING"));
    assert!(GeneratedImage::from_rgb(image::RgbImage::new(0, 1), Duration::ZERO).is_err());
}

#[test]
fn discovery_and_malformed_calls_never_scan_models_or_open_the_pile() {
    let directory = tempfile::tempdir().unwrap();
    let adapter = imagine::mcp::Imagine::with_sources(
        directory.path().join("absent.pile"),
        None,
        sources(directory.path()),
    );
    assert_eq!(adapter.tools().len(), 1);
    for input in [
        "[]",
        "{}",
        r#"{"prompt":"a","prompt":"b"}"#,
        r#"{"prompt":"x","out":"/host/path"}"#,
        r#"{"prompt":"x","model_dir":"/host/path"}"#,
        r#"{"prompt":"x","width":17}"#,
        r#"{"prompt":"x","steps":0}"#,
        r#"{"prompt":"x","remember":"bad time"}"#,
        r#"{"prompt":"x","remember":"2026-09-08T01:00:00..2026-09-08T01:00:00"}"#,
    ] {
        let error = adapter
            .call(
                "imagine_generate",
                input.to_owned().into(),
                &mut Out::new(&mut |_| panic!("invalid input emitted")),
            )
            .unwrap_err();
        assert!(
            error.downcast_ref::<InvalidArguments>().is_some(),
            "{error:#}"
        );
    }
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
}

#[test]
#[cfg(not(feature = "imagine"))]
fn disabled_flux_reports_capability_without_model_or_journal_side_effects() {
    let directory = tempfile::tempdir().unwrap();
    let adapter = imagine::mcp::Imagine::with_sources(
        directory.path().join("absent.pile"),
        None,
        sources(directory.path()),
    );
    let error = adapter
        .call(
            "imagine_generate",
            r#"{"prompt":"@/literal","provider":"flux","remember":"2026-09-08T01:00:00"}"#
                .to_owned()
                .into(),
            &mut Out::new(&mut |_| panic!("no generation")),
        )
        .unwrap_err();
    assert!(error.to_string().contains("`imagine` feature"), "{error:#}");
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
}

/// The default provider is OpenAI. Without a usable bootstrap it fails at
/// storage admission, before any credential, network or journal write.
#[test]
fn default_openai_without_a_bootstrap_fails_before_sending_or_writing() {
    let directory = tempfile::tempdir().unwrap();
    let operations = imagine::Imagine::new(directory.path().join("absent.pile"), None)
        .with_openai_endpoint(imagine::openai::Endpoint::new("http://127.0.0.1:9/v1"))
        .with_unstored_directory(directory.path().join("unstored"));
    let adapter = imagine::mcp::Imagine::from_operations(operations);
    let error = adapter
        .call(
            "imagine_generate",
            r#"{"prompt":"@/literal","remember":"2026-09-08T01:00:00"}"#
                .to_owned()
                .into(),
            &mut Out::new(&mut |_| panic!("no generation")),
        )
        .unwrap_err();
    assert!(
        error.to_string().contains("nothing was generated"),
        "{error:#}"
    );
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
}

#[test]
fn bare_cli_help_does_not_load_models() {
    let result = std::process::Command::new(env!("CARGO_BIN_EXE_imagine"))
        .env_remove("PILE")
        .env_remove("DRIVE_ENDPOINT")
        .output()
        .unwrap();
    assert!(result.status.success());
    assert!(String::from_utf8(result.stdout).unwrap().contains("Usage:"));
}

/// Run the CLI with a prompt that would read a file or stdin, keeping stdin
/// OPEN: a refused generation must exit without waiting for an EOF that an
/// interactive caller may never send. Returns stderr.
fn refused_without_reading_the_prompt(arguments: &[&str]) -> String {
    use std::process::{Command, Stdio};
    let mut child = Command::new(env!("CARGO_BIN_EXE_imagine"))
        .args(arguments)
        .env_remove("DRIVE_ENDPOINT")
        .env_remove("PILE")
        .env_remove("FACULTIES_WORKSPACE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while child.try_wait().unwrap().is_none() {
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("imagine waited for prompt input: {arguments:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
#[cfg(not(feature = "imagine"))]
fn disabled_flux_cli_does_not_read_prompt_files_or_wait_for_stdin() {
    let directory = tempfile::tempdir().unwrap();
    let absent = format!("@{}", directory.path().join("absent.txt").display());
    for prompt in [absent.as_str(), "@-"] {
        let stderr = refused_without_reading_the_prompt(&["--provider", "flux", prompt]);
        assert!(stderr.contains("`imagine` feature"), "{stderr}");
    }
}

#[test]
fn openai_cli_without_a_pile_does_not_read_prompt_files_or_wait_for_stdin() {
    let directory = tempfile::tempdir().unwrap();
    let absent = format!("@{}", directory.path().join("absent.txt").display());
    for prompt in [absent.as_str(), "@-"] {
        let stderr = refused_without_reading_the_prompt(&[prompt]);
        assert!(stderr.contains("needs --pile or PILE"), "{stderr}");
    }
}
