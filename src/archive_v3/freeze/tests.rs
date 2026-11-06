//! The freezer on tiny synthetic Codex rollouts (the G0 and G1 fixtures of
//! the importer's own tests), in temporary directories only; and the
//! ignored real run.

use std::fs;

use serde_json::{json, Value};
use tempfile::TempDir;
use triblespace::core::blob::encodings::UnknownBlob;

use super::*;
use crate::archive_v3::codex;

fn jsonl(records: &[Value]) -> String {
    records
        .iter()
        .map(|record| serde_json::to_string(record).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

/// G0, as in the importer's tests.
fn g0() -> String {
    jsonl(&[
        json!({"id": "11111111-1111-4111-8111-111111111111", "timestamp": "2025-07-07T20:00:00.000Z"}),
        json!({"type": "message", "role": "user",
            "content": [{"type": "input_text", "text": "List the files."}]}),
        json!({"type": "local_shell_call", "id": "lsh_1", "call_id": "call_1", "status": "completed",
            "action": {"type": "exec", "command": ["ls", "-a"], "env": {}, "timeout_ms": 1000,
                "user": null, "working_directory": null}}),
        json!({"type": "function_call_output", "call_id": "call_1",
            "output": "{\"output\":\"a\\nb\\n\",\"metadata\":{\"exit_code\":0,\"duration_seconds\":0.1}}"}),
        json!({"type": "message", "role": "assistant",
            "content": [{"type": "output_text", "text": "Two files: a and b."}]}),
    ])
}

const REASONING: &str = "c2VhbGVkIHJlYXNvbmluZyA_Pn4g-_8=";

/// G1, as in the importer's tests.
fn g1() -> String {
    jsonl(&[
        json!({"id": "22222222-2222-4222-8222-222222222222", "timestamp": "2025-09-01T09:00:00.000Z",
            "instructions": "Be brief.", "git": {"commit_hash": "abc", "branch": "main",
                "repository_url": "git@example:x"}}),
        json!({"record_type": "state"}),
        json!({"type": "message", "id": null, "role": "user",
            "content": [{"type": "input_text", "text": "<user_instructions>\nBe brief.\n</user_instructions>"}]}),
        json!({"type": "message", "id": null, "role": "user",
            "content": [{"type": "input_text", "text": "Run the tests."}]}),
        json!({"type": "reasoning", "id": "rs_1", "summary": [
            {"type": "summary_text", "text": "**Planning** the run."}],
            "content": null, "encrypted_content": REASONING}),
        json!({"type": "function_call", "id": "fc_1", "call_id": "call_2", "name": "shell",
            "arguments": "{\"command\":[\"cargo\",\"test\"],\"timeout\":60000}"}),
        json!({"type": "function_call_output", "call_id": "call_2", "output": "ok"}),
        json!({"type": "reasoning", "id": "rs_2", "summary": [
            {"type": "summary_text", "text": "**Reading** the output."}],
            "content": null, "encrypted_content": null}),
        json!({"type": "message", "id": "msg_1", "role": "assistant",
            "content": [{"type": "output_text", "text": "All tests pass."}]}),
    ])
}

/// G1 with header instructions its user message does not copy: refused at
/// `.instructions`.
fn g1_refused() -> String {
    let source = g1().replace(
        "\"instructions\":\"Be brief.\"",
        "\"instructions\":\"Be long.\"",
    );
    assert_ne!(source, g1());
    source
}

const NOT_JSON: &str = "not json\n";

fn build() -> Build {
    Build {
        faculties_revision: "fixture".to_owned(),
        faculties_dirty: false,
        binary_sha256: sha256(b"fixture binary"),
        cargo_lock_sha256: sha256(b"fixture lock"),
    }
}

/// The fact-set hash computed independently: every raw trible collected,
/// sorted, concatenated, hashed.
fn independent_hash(facts: &TribleSet) -> String {
    let mut raw: Vec<[u8; 64]> = facts.iter().map(|trible| *trible.as_raw()).collect();
    raw.sort_unstable();
    sha256(&raw.concat())
}

fn import_ok(source: &str) -> Fragment {
    codex::import(source.as_bytes()).unwrap_or_else(|failure| panic!("{failure}"))
}

struct Corpus {
    root: PathBuf,
    generation: PathBuf,
    _directory: TempDir,
}

impl Corpus {
    fn new(files: &[(&str, &str)]) -> Self {
        let directory = TempDir::new().unwrap();
        let root = directory.path().join("corpus");
        fs::create_dir(&root).unwrap();
        for (name, text) in files {
            fs::write(root.join(name), text).unwrap();
        }
        Corpus {
            root,
            generation: directory.path().join("generation"),
            _directory: directory,
        }
    }

    fn start(&self) -> Result<Generation> {
        let source = Source::Corpus(self.root.clone());
        Generation::open(&self.generation, Format::Codex, build(), Some(source))
    }

    fn resume(&self) -> Result<Generation> {
        Generation::open(&self.generation, Format::Codex, build(), None)
    }
}

fn freeze(generation: Result<Generation>) -> Tally {
    let mut generation = generation.unwrap();
    let tally = generation.run().unwrap();
    generation.close().unwrap();
    tally
}

#[test]
fn sha256_is_sha256() {
    assert_eq!(
        sha256(b""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        sha256(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn an_accepted_file_records_its_fact_hash_and_the_pile_holds_its_facts() {
    let source = g0();
    let corpus = Corpus::new(&[("rollout-a.jsonl", source.as_str())]);
    let tally = freeze(corpus.start());
    assert_eq!((tally.accepted, tally.refused), (1, 0));
    assert_eq!(tally.bytes_read, source.len() as u64);
    let records = records(&corpus.generation).unwrap();
    assert_eq!(records.len(), 1);
    let record = &records[0];
    let mut fragment = import_ok(&source);
    assert_eq!(record["outcome"], "accepted");
    assert_eq!(record["sha256"], sha256(source.as_bytes()));
    assert_eq!(record["manifest_size"], source.len());
    assert_eq!(record["read_size"], source.len());
    assert_eq!(record["facts"], fragment.facts().len());
    assert_eq!(record["fact_sha256"], independent_hash(fragment.facts()));
    assert_eq!(record["fact_sha256"], fact_sha256(fragment.facts()));

    // The commit names the facts' archive; the pile holds it and every blob.
    let archive = <TribleSet as IntoBlob<SimpleArchive>>::to_blob(fragment.facts().clone());
    assert_eq!(record["commit_data"], hex::encode(archive.get_handle().raw));
    let mut pile = open_pile_strict(&corpus.generation.join(PILE)).unwrap();
    let snapshot = pile.snapshot().unwrap();
    let stored: TribleSet = snapshot.get(archive.get_handle()).unwrap();
    assert_eq!(&stored, fragment.facts());
    let blobs = fragment.blobs_mut().snapshot().unwrap();
    assert!(!blobs.is_empty());
    assert_eq!(record["blobs"], blobs.len());
    for (handle, blob) in blobs.iter() {
        let held = snapshot
            .get::<anybytes::Bytes, UnknownBlob>(handle)
            .unwrap();
        assert_eq!(&held[..], &blob.bytes[..]);
    }
    drop(snapshot);
    pile.close().unwrap();

    // The header states the identity and the method.
    let header: Header =
        serde_json::from_slice(&fs::read(corpus.generation.join(HEADER)).unwrap()).unwrap();
    assert_eq!(header.identity.build, build());
    assert_eq!(header.identity.format, "codex");
    assert_eq!(header.identity.fact_hash, FACT_HASH);
    assert_eq!(header.identity.manifest_entries, 1);
    let manifest = fs::read(corpus.generation.join(MANIFEST)).unwrap();
    assert_eq!(header.identity.manifest_sha256, sha256(&manifest));
}

#[test]
fn a_refused_file_records_the_importers_path_and_reason() {
    let refused = g1_refused();
    let corpus = Corpus::new(&[
        ("rollout-a.jsonl", refused.as_str()),
        ("rollout-b.jsonl", NOT_JSON),
    ]);
    let tally = freeze(corpus.start());
    assert_eq!(
        (tally.accepted, tally.refused, tally.operational),
        (0, 2, 0)
    );
    let records = records(&corpus.generation).unwrap();
    assert_eq!(records.len(), 2);
    for (record, source) in records.iter().zip([refused.as_str(), NOT_JSON]) {
        let failure = codex::import(source.as_bytes()).err().expect("refused");
        assert_eq!(record["outcome"], "refused");
        assert_eq!(record["failure_path"], failure.path);
        assert_eq!(record["reason"], failure.reason);
        assert_eq!(record["sha256"], sha256(source.as_bytes()));
        assert_eq!(record["read_size"], source.len());
        assert!(record.get("commit_data").is_none());
    }
    assert_eq!(records[0]["failure_path"], ".instructions");
    assert_eq!(records[1]["failure_path"], "<document>");
    assert_eq!(records[1]["reason"], "a line is not JSON");
}

#[test]
fn a_file_that_grows_after_the_manifest_records_both_sizes() {
    let source = g0();
    let corpus = Corpus::new(&[("rollout-a.jsonl", source.as_str())]);
    let entries = enumerate(&corpus.root).unwrap();
    let manifest = corpus.root.with_file_name("manifest.jsonl");
    fs::write(&manifest, manifest_bytes(&entries)).unwrap();
    let grown = source.clone() + "{\"record_type\":\"state\"}\n";
    fs::write(&entries[0].path, &grown).unwrap();

    let opened = Generation::open(
        &corpus.generation,
        Format::Codex,
        build(),
        Some(Source::Manifest(manifest.clone())),
    );
    let tally = freeze(opened);
    assert_eq!((tally.changed, tally.accepted), (1, 0));
    let records = records(&corpus.generation).unwrap();
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(record["outcome"], "changed");
    assert_eq!(record["path"], entries[0].path);
    assert_eq!(record["manifest_size"], source.len());
    assert_eq!(record["read_size"], grown.len());
    assert_eq!(record["sha256"], sha256(grown.as_bytes()));
    assert!(record.get("facts").is_none());
    // The generation's manifest is the given one, byte for byte.
    assert_eq!(
        fs::read(corpus.generation.join(MANIFEST)).unwrap(),
        fs::read(&manifest).unwrap()
    );
}

#[test]
fn a_resume_skips_a_file_only_when_its_fresh_sha256_matches() {
    let corpus = Corpus::new(&[
        ("rollout-a.jsonl", g0().as_str()),
        ("rollout-b.jsonl", g1().as_str()),
    ]);
    let first = freeze(corpus.start());
    assert_eq!(first.accepted, 2);
    let again = freeze(corpus.resume());
    assert_eq!((again.skipped, again.accepted), (2, 0));

    // Other bytes under the same path, size and mtime: not done.
    let path = corpus.root.join("rollout-b.jsonl");
    let modified = fs::metadata(&path).unwrap().modified().unwrap();
    let edited = g1().replace("All tests pass.", "All tests fail.");
    assert_eq!(edited.len(), g1().len());
    fs::write(&path, &edited).unwrap();
    fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(modified)
        .unwrap();
    let resumed = freeze(corpus.resume());
    assert_eq!((resumed.skipped, resumed.accepted), (1, 1));

    let records = records(&corpus.generation).unwrap();
    assert_eq!(records.len(), 3);
    let (before, after) = (&records[1], &records[2]);
    assert_eq!(after["path"], before["path"]);
    assert_eq!(after["outcome"], "accepted");
    assert_eq!(after["sha256"], sha256(edited.as_bytes()));
    assert_ne!(after["sha256"], before["sha256"]);
    assert_eq!(
        after["fact_sha256"],
        independent_hash(import_ok(&edited).facts())
    );
    assert_ne!(after["fact_sha256"], before["fact_sha256"]);
}

#[test]
fn a_resume_examines_again_a_file_whose_record_is_not_a_verdict() {
    let corpus = Corpus::new(&[
        ("rollout-a.jsonl", g0().as_str()),
        ("rollout-b.jsonl", g1().as_str()),
    ]);
    assert_eq!(freeze(corpus.start()).accepted, 2);
    // Rewrite the second record as an operational one, same path and sha256.
    let path = corpus.generation.join(RECORDS);
    let mut lines: Vec<Value> = records(&corpus.generation).unwrap();
    lines[1]["outcome"] = json!("operational");
    let text: String = lines.iter().map(|line| format!("{line}\n")).collect();
    fs::write(&path, text).unwrap();
    let resumed = freeze(corpus.resume());
    assert_eq!((resumed.skipped, resumed.accepted), (1, 1));
    let records = records(&corpus.generation).unwrap();
    assert_eq!(records.len(), 3);
    assert_eq!(records[2]["path"], records[1]["path"]);
    assert_eq!(records[2]["outcome"], "accepted");
}

#[test]
fn an_interrupted_run_resumes_to_the_same_records() {
    let corpus = Corpus::new(&[
        ("rollout-a.jsonl", g0().as_str()),
        ("rollout-b.jsonl", g1().as_str()),
        ("rollout-c.jsonl", NOT_JSON),
    ]);
    freeze(corpus.start());
    let path = corpus.generation.join(RECORDS);
    let whole = fs::read(&path).unwrap();
    let ends: Vec<usize> = whole
        .iter()
        .enumerate()
        .filter(|(_, byte)| **byte == b'\n')
        .map(|(at, _)| at + 1)
        .collect();
    assert_eq!(ends.len(), 3);
    // One record survives, and the second was torn mid-append.
    let torn = ends[0] + (ends[1] - ends[0]) / 2;
    fs::write(&path, &whole[..torn]).unwrap();

    let resumed = freeze(corpus.resume());
    assert_eq!(
        (resumed.skipped, resumed.accepted, resumed.refused),
        (1, 1, 1)
    );
    assert_eq!(fs::read(&path).unwrap(), whole);
}

#[test]
fn the_lock_refuses_a_second_concurrent_run() {
    let corpus = Corpus::new(&[("rollout-a.jsonl", g0().as_str())]);
    let first = corpus.start().unwrap();
    let error = corpus.resume().err().expect("a second run is refused");
    assert!(
        format!("{error:#}").contains("another run holds"),
        "{error:#}"
    );
    first.close().unwrap();
    // The lock goes with the run.
    freeze(corpus.resume());
}

#[test]
fn a_resume_under_another_identity_is_refused() {
    let corpus = Corpus::new(&[("rollout-a.jsonl", g0().as_str())]);
    freeze(corpus.start());
    let mut other = build();
    other.binary_sha256 = sha256(b"another binary");
    for (format, identity) in [(Format::Codex, other), (Format::ClaudeCode, build())] {
        let error = Generation::open(&corpus.generation, format, identity, None)
            .err()
            .expect("refused");
        assert!(
            format!("{error:#}").contains("not the generation's identity"),
            "{error:#}"
        );
    }
    let error = corpus.start().err().expect("a resume takes no corpus root");
    assert!(format!("{error:#}").contains("AV3_CORPUS"), "{error:#}");

    // A directory that is not a generation gets nothing written into it.
    let stranger = TempDir::new().unwrap();
    let source = Some(Source::Corpus(corpus.root.clone()));
    assert!(Generation::open(stranger.path(), Format::Codex, build(), source).is_err());
    assert_eq!(fs::read_dir(stranger.path()).unwrap().count(), 0);
}

#[test]
#[ignore = "freezes the corpus under AV3_CORPUS or in AV3_MANIFEST into AV3_FREEZE_DIR; prints counts only"]
fn freeze_real_corpus() {
    let format =
        Format::parse(&std::env::var("AV3_FORMAT").expect("AV3_FORMAT is codex or claude_code"))
            .unwrap();
    let directory = PathBuf::from(
        std::env::var_os("AV3_FREEZE_DIR").expect("AV3_FREEZE_DIR names the generation"),
    );
    let source = match (
        std::env::var_os("AV3_CORPUS"),
        std::env::var_os("AV3_MANIFEST"),
    ) {
        (Some(_), Some(_)) => panic!("set AV3_CORPUS or AV3_MANIFEST, not both"),
        (Some(root), None) => Some(Source::Corpus(root.into())),
        (None, Some(manifest)) => Some(Source::Manifest(manifest.into())),
        (None, None) => None,
    };
    let build = Build::current().unwrap();
    eprintln!("freeze: {build:?}");
    let mut generation = Generation::open(&directory, format, build, source).unwrap();
    let tally = generation.run().unwrap();
    generation.close().unwrap();
    // The completion record a driver gates on: written only after the
    // last record and the pile are synced and closed.
    let completed = json!({
        "completed": chrono::Utc::now().to_rfc3339(),
        "accepted": tally.accepted,
        "refused": tally.refused,
        "changed": tally.changed,
        "operational": tally.operational,
        "skipped": tally.skipped,
        "bytes_read": tally.bytes_read,
    });
    append_to(&directory, RUNS, &completed).unwrap();
    println!("freeze_complete {completed}");
}
