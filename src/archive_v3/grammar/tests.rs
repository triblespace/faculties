//! The compiled grammar against the gate's walker, on synthetic transcripts,
//! and (ignored) on the real sample, printing only paths, kinds and counts.

use std::collections::BTreeMap;

use serde_json::{json, Value};
use triblespace::core::blob::MemoryBlobStore;
use triblespace::core::import::json_tree::JsonTreeImporter;
use triblespace::core::repo::BlobStore;

use super::*;
use crate::archive_v3::fields::claude_code::TABLE;
use crate::archive_v3::gate::{self, Kind};

/// The walker's row kinds that are classification, the half compiled here.
const CLASSIFICATION: &[Kind] = &[
    Kind::Unknown,
    Kind::UnknownValue,
    Kind::Empty,
    Kind::Open,
    Kind::Table,
];

fn jsonl(records: &[Value]) -> String {
    records
        .iter()
        .map(|record| serde_json::to_string(record).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

fn stamp(second: u32) -> String {
    format!("2026-03-01T10:00:{second:02}.000Z")
}

fn user(uuid: &str, parent: Option<&str>, second: u32, content: Value) -> Value {
    json!({
        "type": "user", "sessionId": "s-1", "uuid": uuid, "parentUuid": parent,
        "timestamp": stamp(second), "cwd": "/synthetic", "version": "0.0.0",
        "isSidechain": false, "userType": "external",
        "message": {"role": "user", "content": content}
    })
}

fn assistant(uuid: &str, parent: &str, second: u32, id: &str, block: Value, stop: Value) -> Value {
    json!({
        "type": "assistant", "sessionId": "s-1", "uuid": uuid, "parentUuid": parent,
        "timestamp": stamp(second), "requestId": format!("req-{id}"),
        "message": {"id": id, "type": "message", "role": "assistant",
            "model": "claude-opus-4-6", "content": [block],
            "stop_reason": stop, "stop_sequence": null, "usage": {"input_tokens": 1}}
    })
}

fn said(text: &str) -> Value {
    json!({"type": "text", "text": text})
}

fn call(id: &str, name: &str, input: Value) -> Value {
    json!({"type": "tool_use", "id": id, "name": name, "input": input, "caller": {"type": "direct"}})
}

fn result(call: &str, content: Value) -> Value {
    json!([{"type": "tool_result", "tool_use_id": call, "content": content, "is_error": false}])
}

/// The walker's classification rows for a unit, as (path, kind).
fn walker_rows(source: &[u8]) -> Vec<(String, String)> {
    gate::walk::walk(&TABLE, source)
        .rows
        .into_iter()
        .filter(|(_, kind, _)| CLASSIFICATION.contains(kind))
        .map(|(path, kind, _)| (path, kind.to_string()))
        .collect()
}

/// The grammar's findings for a unit, or none when it has one cover. The
/// unit is its lines as one JSON array; a line that is not JSON is `None`.
fn grammar_findings(grammar: &Grammar<Cat>, source: &[u8]) -> Option<Vec<String>> {
    grammar_findings_timed(grammar, source, &mut std::time::Duration::ZERO)
}

/// [`grammar_findings`], adding the time spent importing the unit to
/// `importing`.
fn grammar_findings_timed(
    grammar: &Grammar<Cat>,
    source: &[u8],
    importing: &mut std::time::Duration,
) -> Option<Vec<String>> {
    let started = std::time::Instant::now();
    let text = std::str::from_utf8(source).ok()?;
    let records: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let array = format!("[{}]", records.join(","));
    let mut store = MemoryBlobStore::new();
    let fragment = JsonTreeImporter::<_>::new(&mut store, None)
        .import_str(&array)
        .ok()?;
    let root = fragment.root()?;
    let blobs = fragment.blobs().clone().snapshot().ok()?;
    *importing += started.elapsed();
    match grammar.cover(Unit::new(fragment.facts(), &blobs), root, &lines()) {
        Ok(_) => Some(Vec::new()),
        // Only the normalized path and the kind of problem: an unclaimed
        // finding names source keys, which may be ids or content.
        Err(findings) => Some(
            findings
                .into_iter()
                .map(|f| {
                    let kind = format!("{:?}", f.problem);
                    let kind = kind.split([' ', '{']).next().unwrap_or("").to_owned();
                    format!("{}\t{kind}", gate::vocabulary(&TABLE, &f.category.path))
                })
                .collect(),
        ),
    }
}

fn grammar() -> Grammar<Cat> {
    compile(&TABLE).expect("the Claude Code table compiles")
}

fn agree(source: &str) -> (Vec<(String, String)>, Vec<String>) {
    let rows = walker_rows(source.as_bytes());
    let findings = grammar_findings(&grammar(), source.as_bytes()).expect("synthetic JSON");
    assert_eq!(
        rows.is_empty(),
        findings.is_empty(),
        "walker rows {rows:?}\ngrammar findings {findings:?}"
    );
    (rows, findings)
}

fn transcript(extra: Value, block: Value, stop: Value) -> String {
    let mut first = user("u1", None, 0, json!([said("Look.")]));
    if let (Value::Object(record), Value::Object(more)) = (&mut first, extra) {
        record.extend(more);
    }
    jsonl(&[
        first,
        assistant("a1", "u1", 1, "m1", block, json!(null)),
        assistant(
            "a2",
            "a1",
            2,
            "m1",
            call("t1", "Read", json!({"file_path": "/x"})),
            json!("tool_use"),
        ),
        user("u2", Some("a2"), 3, result("t1", json!("ok"))),
        assistant("a3", "u2", 4, "m2", said("Done."), stop),
    ])
}

#[test]
fn a_well_formed_transcript_classifies_in_both() {
    let (rows, findings) = agree(&transcript(json!({}), said("I see."), json!("end_turn")));
    assert!(rows.is_empty() && findings.is_empty());
}

#[test]
fn an_unknown_record_field_fails_in_both() {
    let (rows, findings) = agree(&transcript(
        json!({"zzzUnknownField": 1}),
        said("I see."),
        json!("end_turn"),
    ));
    assert!(!rows.is_empty() && !findings.is_empty());
}

#[test]
fn an_unknown_block_shape_fails_in_both() {
    let (rows, _) = agree(&transcript(
        json!({}),
        json!({"type": "hologram", "beam": "x"}),
        json!("end_turn"),
    ));
    assert!(!rows.is_empty());
}

#[test]
fn a_stop_reason_outside_the_vocabulary_fails_in_both() {
    let (rows, _) = agree(&transcript(json!({}), said("I see."), json!("bored")));
    assert!(!rows.is_empty());
}

#[test]
#[ignore = "reads real transcripts named by AV3_SAMPLE; prints no values"]
fn real_sample_parity() {
    let list = std::env::var("AV3_SAMPLE").expect("AV3_SAMPLE names a file list");
    let files: Vec<String> = std::fs::read_to_string(list)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    let g = grammar();
    let mut table: BTreeMap<(bool, bool), usize> = BTreeMap::new();
    let mut walker_only: BTreeMap<String, usize> = BTreeMap::new();
    let mut grammar_only: BTreeMap<String, usize> = BTreeMap::new();
    let mut unreadable = 0;
    let (mut walking, mut covering) = (std::time::Duration::ZERO, std::time::Duration::ZERO);
    let mut importing = std::time::Duration::ZERO;
    for file in &files {
        let Ok(bytes) = std::fs::read(file) else {
            continue;
        };
        let started = std::time::Instant::now();
        let rows = walker_rows(&bytes);
        walking += started.elapsed();
        let started = std::time::Instant::now();
        let findings = grammar_findings_timed(&g, &bytes, &mut importing);
        covering += started.elapsed();
        let Some(findings) = findings else {
            unreadable += 1;
            continue;
        };
        let walker_fails = !rows.is_empty();
        let grammar_fails = !findings.is_empty();
        *table.entry((walker_fails, grammar_fails)).or_default() += 1;
        if walker_fails && !grammar_fails {
            for (path, kind) in rows.iter().take(3) {
                *walker_only
                    .entry(format!("{}\t{kind}", gate::vocabulary(&TABLE, path)))
                    .or_default() += 1;
            }
        }
        if grammar_fails && !walker_fails {
            for finding in findings.iter().take(3) {
                *grammar_only.entry(finding.clone()).or_default() += 1;
            }
        }
    }
    println!("files {} unreadable-as-json {unreadable}", files.len());
    // Wall time on this machine, summed over the files: the walker, and the
    // grammar including the json_tree import of each unit.
    println!(
        "walker {:.1} s, grammar with import {:.1} s, of which import {:.1} s",
        walking.as_secs_f64(),
        covering.as_secs_f64(),
        importing.as_secs_f64()
    );
    for ((w, gr), n) in &table {
        println!("walker-fails {w} grammar-fails {gr}: {n}");
    }
    println!("-- walker fails, grammar accepts: first rows (files, path, kind)");
    for (row, n) in &walker_only {
        println!("{n}\t{row}");
    }
    println!("-- grammar fails, walker accepts: first findings (files, finding)");
    for (row, n) in &grammar_only {
        println!("{n}\t{row}");
    }
}
