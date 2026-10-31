//! Care operations and explicit CLI/MCP boundaries over temporary native piles.

use anybytes::Bytes;
use anyhow::Result;
use faculties::care::{self, AddInput, Care};
use faculties::mcp::{Faculty, InvalidArguments, Server};
use faculties::out::{Out, Part};
use faculties::relations::{ProfileInput, Relations};
use faculties::storage::initialize_signer;
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

struct Fixture {
    _directory: tempfile::TempDir,
    pile: PathBuf,
    key: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let pile = directory.path().join("care.pile");
        let key = directory.path().join("care.key");
        fs::File::create(&pile).unwrap();
        initialize_signer(&pile, Some(&key)).unwrap();
        Self {
            _directory: directory,
            pile,
            key,
        }
    }
    fn care(&self) -> Care {
        Care::new(self.pile.clone(), Some(self.key.clone()))
    }
    fn mcp(&self) -> care::mcp::Care {
        care::mcp::Care::new(self.pile.clone(), Some(self.key.clone()))
    }
    fn person(&self, label: &str) -> String {
        let relations = Relations::new(self.pile.clone(), Some(self.key.clone()));
        let person = relations
            .add(
                ProfileInput {
                    label: label.into(),
                    ..Default::default()
                },
                None,
                &[],
            )
            .unwrap();
        format!("{:x}", person.person)
    }
    fn cli(&self, arguments: &[&str]) -> std::process::Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_care"));
        clean_child(&mut command);
        command
            .arg("--pile")
            .arg(&self.pile)
            .arg("--key")
            .arg(&self.key)
            .args(arguments)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }
    fn cli_ok(&self, arguments: &[&str]) -> String {
        let output = self.cli(arguments);
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).unwrap()
    }
}
fn clean_child(command: &mut Command) {
    for (name, _) in std::env::vars_os() {
        let text = name.to_string_lossy();
        if text.starts_with("TRIBLESPACE_") || matches!(text.as_ref(), "PILE" | "PERSONA") {
            command.env_remove(name);
        }
    }
}
fn collect(execute: impl FnOnce(&mut Out<'_>) -> Result<()>) -> Result<String> {
    let mut result = String::new();
    execute(&mut Out::new(&mut |part| {
        match part {
            Part::Text { text } => result.push_str(&text),
            other => panic!("unexpected output {other:?}"),
        }
        Ok(())
    }))?;
    Ok(result)
}
fn call(faculty: &care::mcp::Care, tool: &str, args: Value) -> Result<String> {
    collect(|out| faculty.call(tool, Bytes::from(serde_json::to_vec(&args).unwrap()), out))
}
fn reading(subject: &str, kind: &str, value: f64, unit: &str) -> AddInput {
    AddInput {
        subject: subject.into(),
        kind: kind.into(),
        value,
        unit: unit.into(),
        observed: "2026-10-30T21:29".into(),
        reported: Some("2026-10-30T21:31".into()),
        method: "measured-device".into(),
        ..Default::default()
    }
}

#[test]
fn cli_mcp_and_native_reads_agree_and_corrections_replace_their_line() {
    let fixture = Fixture::new();
    let id = fixture.person("Example Patient");
    let operations = fixture.care();
    let systolic = operations
        .add(&reading("Example Patient", "systolic", 122.0, "mm[Hg]"))
        .unwrap();
    assert_eq!(format!("{:x}", systolic.subject), id);
    operations
        .add(&reading(&id, "8462-4", 77.0, "mmHg"))
        .unwrap();
    operations
        .add(&AddInput {
            observed: "2026-10-30T20:00..2026-10-30T21:00".into(),
            method: "reported-by-subject".into(),
            note: Some("water, \"about\" a glass".into()),
            low: Some(150.0),
            high: Some(250.0),
            ..reading(&id, "intake", 200.0, "mL")
        })
        .unwrap();
    let faculty = fixture.mcp();
    let corrected = call(
        &faculty,
        "care_add",
        json!({"subject": id, "kind": "systolic", "value": 112, "unit": "mm[Hg]",
               "observed": "2026-10-30T21:29", "reported": "2026-10-30T21:40",
               "method": "measured-device",
               "supersedes": [format!("{:x}", systolic.observation)]}),
    )
    .unwrap();
    assert!(corrected.starts_with("observation "));

    let table = operations.table(&id, None, None).unwrap();
    assert_eq!(table.unreadable, 0);
    assert_eq!(
        table.rows.len(),
        3,
        "the corrected reading replaces its line"
    );
    let values: Vec<f64> = table.rows.iter().map(|row| row.value).collect();
    assert!(values.contains(&112.0) && !values.contains(&122.0));
    let intake = table.rows.iter().find(|row| row.unit == "mL").unwrap();
    assert_eq!(intake.loinc, "9108-2");
    assert_eq!((intake.low, intake.high), (Some(150.0), Some(250.0)));
    assert_eq!(intake.note.as_deref(), Some("water, \"about\" a glass"));
    assert_eq!(intake.method, "reported-by-subject");

    let cli_table = fixture.cli_ok(&["table", "Example Patient"]);
    assert_eq!(
        cli_table,
        call(&faculty, "care_table", json!({"subject": id})).unwrap()
    );
    assert!(cli_table.contains("[corrected]"));
    assert!(
        cli_table.contains("2026-10-30T20:00Z..2026-10-30T21:00Z  fluid intake  200 (150..250) mL")
    );
    assert_eq!(
        fixture.cli_ok(&["table", &id, "--kind", "pain"]),
        format!("observations for {id}\n- (none readable)\n")
    );

    let csv = fixture.cli_ok(&["export", &id, "--csv"]);
    assert_eq!(
        csv,
        call(&faculty, "care_export", json!({"subject": id})).unwrap()
    );
    let lines: Vec<&str> = csv.lines().collect();
    assert_eq!(lines.len(), 4);
    assert!(lines[0].starts_with("observation,observed_at,reported_at,kind,loinc,value"));
    assert!(csv.contains(",\"water, \"\"about\"\" a glass\"\n"));
    assert!(csv.contains(&format!("{:x}", systolic.observation)));

    let since = fixture.cli_ok(&["export", &id, "--csv", "--since", "2026-10-30T21:10"]);
    assert_eq!(
        since.lines().count(),
        3,
        "the 20:00..21:00 intake ends before --since"
    );
}

#[test]
fn records_are_sealed_secrets_and_no_clinical_text_reaches_the_pile() {
    let fixture = Fixture::new();
    let id = fixture.person("Sealed Patient");
    fixture
        .care()
        .add(&AddInput {
            note: Some("synthetic-clinical-sentinel".into()),
            ..reading(&id, "pain", 7.0, "{score}")
        })
        .unwrap();
    let bytes = fs::read(&fixture.pile).unwrap();
    for needle in [
        &b"synthetic-clinical-sentinel"[..],
        b"pain severity",
        b"72514-3",
        b"{score}",
    ] {
        assert!(
            !bytes.windows(needle.len()).any(|window| window == needle),
            "{} reached the pile in plaintext",
            String::from_utf8_lossy(needle)
        );
    }
    // The record, note inside, is ONE structured Secrets version: it shows in
    // `secrets items` under its public label, and not in `secrets list`.
    let secrets = faculties::secrets::Secrets::new(fixture.pile.clone(), Some(fixture.key.clone()));
    assert!(secrets.list().unwrap().is_empty());
    let names: Vec<Option<String>> = secrets
        .list_items()
        .unwrap()
        .into_iter()
        .map(|row| row.name)
        .collect();
    assert_eq!(names, [Some(care::RECORD_LABEL.to_owned())]);
    let table = fixture.care().table(&id, None, None).unwrap();
    assert_eq!(
        table.rows[0].note.as_deref(),
        Some("synthetic-clinical-sentinel")
    );
}

#[test]
fn unknown_subjects_and_vocabulary_are_refused_before_writing() {
    let fixture = Fixture::new();
    let id = fixture.person("Known");
    let operations = fixture.care();
    assert!(operations
        .add(&reading("Nobody", "systolic", 1.0, "mm[Hg]"))
        .is_err());
    assert!(operations
        .add(&reading(&id, "glucose", 1.0, "mm[Hg]"))
        .is_err());
    assert!(operations
        .add(&reading(&id, "systolic", f64::NAN, "mm[Hg]"))
        .is_err());
    assert!(operations.table(&id, None, None).unwrap().rows.is_empty());
}

#[test]
fn discovery_help_and_bad_arguments_do_not_open_storage() {
    let directory = tempfile::tempdir().unwrap();
    let pile = directory.path().join("absent.pile");
    let faculty = care::mcp::Care::new(pile.clone(), None);
    Server::new(&[&faculty]).unwrap();
    assert_eq!(
        faculty
            .tools()
            .iter()
            .map(|tool| tool.name)
            .collect::<Vec<_>>(),
        ["care_add", "care_table", "care_grant", "care_export"]
    );
    for (tool, raw) in [
        ("care_add", r#"{"subject":"x"}"#),
        (
            "care_add",
            r#"{"subject":"x","kind":"pain","value":"7","unit":"{score}","observed":"2026-10-30T21:00","method":"reported"}"#,
        ),
        ("care_table", r#"{"subject":"x","pile":"/elsewhere"}"#),
        ("care_export", r#"{}"#),
    ] {
        let error = collect(|out| faculty.call(tool, Bytes::from(raw.as_bytes().to_vec()), out))
            .unwrap_err();
        assert!(error.is::<InvalidArguments>(), "{tool}: {error:#}");
    }
    let mut command = Command::new(env!("CARGO_BIN_EXE_care"));
    clean_child(&mut command);
    let output = command.output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8(output.stdout)
        .unwrap()
        .contains("Usage: care"));
    assert!(!pile.exists());
}
