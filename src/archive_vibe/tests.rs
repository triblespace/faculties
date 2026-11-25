use std::collections::BTreeSet;

use serde_json::{json, Value};
use triblespace::prelude::*;

use super::*;
use crate::schemas::blockdag::{block, content_fact, source_projection};

pub(crate) const SESSION: &str = "aaaaaaaa-aaaa-4aaa-8aaa-000000000001";
const PREDECESSOR: &str = "aaaaaaaa-aaaa-4aaa-8aaa-000000000002";

fn write_json(path: &Path, json: &Value) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(json).unwrap();
    bytes.push(b'\n');
    fs::write(path, &bytes).unwrap();
    bytes.pop();
    bytes
}

pub(crate) fn write_current(root: &Path, session: &str, sequence: u64) {
    let name = format!("{sequence:016}");
    let generation = root.join("generations").join(&name);
    fs::create_dir_all(&generation).unwrap();
    fs::create_dir_all(root.join("journal")).unwrap();
    let first = sequence + 1;
    let manifest = write_json(
        &generation.join("manifest.json"),
        &json!({
            "generation": name,
            "manifest_version": 1,
            "recovery_journal_segment": {
                "first_sequence": first,
                "path": format!("journal/{first:016}.jsonl"),
            },
            "session_id": session,
            "snapshot_sequence": sequence,
        }),
    );
    write_json(
        &root.join("CURRENT"),
        &json!({
            "generation": name,
            "manifest_sha256": hex::encode(Sha256::digest(&manifest)),
            "session_id": session,
            "snapshot_sequence": sequence,
            "store_format": STORE_FORMAT,
            "store_format_minor": 10,
        }),
    );
}

fn write_export(root: &Path, session: &str) {
    fs::create_dir_all(root).unwrap();
    write_json(
        &root.join("export.json"),
        &json!({
            "schema_version": 1,
            "session_id": session,
            "journal_dir": "session",
            "outcome": "finished",
            "config": { "placeholder": "export metadata is outside the graph" },
        }),
    );
}

type Record = (&'static str, Value);

fn segment(root: &Path, first: u64, records: &[Record], tail: &[u8]) -> PathBuf {
    let path = root.join("journal").join(format!("{first:016}.jsonl"));
    let mut previous: Option<String> = None;
    let mut bytes = Vec::new();
    let canonical = |value: Value| {
        archive_source::canonical_dynamic_json(Bytes::from_source(
            serde_json::to_vec(&value).unwrap(),
        ))
        .unwrap()
    };
    for (index, (kind, payload)) in records.iter().enumerate() {
        let mut line = json!({
            "payload": payload,
            "previous_record_sha256": previous,
            "recovery_journal_record_version": 1,
            "sequence": first + index as u64,
            "type": kind,
        });
        let hash = hex::encode(Sha256::digest(canonical(line.clone())));
        line["record_sha256"] = json!(hash);
        bytes.extend_from_slice(canonical(line).as_bytes());
        bytes.push(b'\n');
        previous = Some(hash);
    }
    bytes.extend_from_slice(tail);
    fs::write(&path, bytes).unwrap();
    path
}

fn intent(id: &str, messages: Value) -> Record {
    (
        "action_intent",
        json!({
            "action_id": id,
            "kind": "completion",
            "request": {
                "action_id": id,
                "type": "llm_call",
                "purpose": "agent",
                "model_input": {
                    "messages": messages,
                    "tool_catalog": {"type": "keep", "revision": 0},
                },
            },
        }),
    )
}

fn success(id: &str, parts: Value) -> Record {
    (
        "action_result",
        json!({
            "action_id": id,
            "result": {
                "action_id": id,
                "type": "completion_succeeded",
                "result": {"parts": parts},
            },
        }),
    )
}

fn failure(id: &str) -> Record {
    (
        "action_result",
        json!({
            "action_id": id,
            "result": {
                "action_id": id,
                "type": "completion_failed",
                "error": { "message": "failure metadata is outside the graph" },
            },
        }),
    )
}

fn initial() -> Record {
    intent(
        "first-action",
        json!({
            "type": "replace",
            "revision": 1,
            "messages": [
                {"role": "system", "content": [{"type": "text", "text": "synthetic rules"}]},
                {"role": "user", "content": [{"type": "text", "text": "synthetic question"}]},
            ]
        }),
    )
}

fn first_completion() -> Record {
    success(
        "first-action",
        json!([
            {"type": "reasoning", "content": [{"type": "text", "text": "a thought"}]},
            {"type": "tool_call", "id": "tool-call-id", "name": "lookup", "arguments_json": "{\"b\":2,\"a\":1}"}
        ]),
    )
}

fn second_intent() -> Record {
    intent(
        "second-action",
        json!({
            "type": "append",
            "base_revision": 1,
            "revision": 3,
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "reasoning", "content": [{"type": "text", "text": "a thought"}]},
                    {"type": "tool_call", "id": "tool-call-id", "name": "lookup",
                        "arguments": {"type":"json", "raw": "{ \"a\": 1, \"b\": 2 }", "value": {"a":1,"b":2}}}
                ]},
                {"role": "tool", "name": "lookup", "tool_call_id": "tool-call-id", "outcome": "success",
                    "content": [{"type": "text", "text": "held tool reply"}]}
            ]
        }),
    )
}

fn answer() -> Record {
    success(
        "second-action",
        json!([{"type": "text", "text": "synthetic answer"}]),
    )
}

pub(crate) fn finished_native(root: &Path) {
    write_current(root, SESSION, 4);
    segment(
        root,
        1,
        &[initial(), first_completion(), second_intent(), answer()],
        b"",
    );
    segment(root, 5, &[], b"");
}

fn project(path: &Path) -> (ProjectionSummary, Fragment) {
    let mut fragment = Fragment::empty();
    let stats = project_path(path, |piece| {
        fragment += piece.fragment;
        Ok(())
    })
    .unwrap();
    (stats, fragment)
}

fn nodes(fragment: &Fragment) -> BTreeSet<Id> {
    find!(node: Id, pattern!(fragment, [{ ?node @ block::holds: _?fact }])).collect()
}

#[test]
fn native_prefix_and_export_converge_across_journal_rotation() {
    let temp = tempfile::tempdir().unwrap();
    let native = temp.path().join(SESSION);
    write_current(&native, SESSION, 0);
    segment(
        &native,
        1,
        &[initial(), first_completion(), second_intent()],
        b"{\"pending\":",
    );
    let (partial, prefix) = project(&native);
    assert_eq!(partial.coverage(), "partial");
    assert_eq!(partial.completed_calls, 1);
    assert_eq!(partial.pending_calls, 1);
    assert_eq!(partial.trailing_bytes_held, 11);
    assert_eq!(nodes(&prefix).len(), 4);

    // The two sides of a completion may reside in different retained files.
    // The writer can rotate away from an incomplete final line after recovery.
    write_current(&native, SESSION, 3);
    segment(&native, 4, &[answer()], b"");
    let (complete_native, grown) = project(&native);
    assert_eq!(complete_native.completed_calls, 2);
    assert_eq!(complete_native.unmatched_assistant_inputs, 0);
    assert_eq!(complete_native.stats.unresolved_tool_results, 0);
    assert_eq!(nodes(&grown).len(), 6);
    assert!(nodes(&prefix).is_subset(&nodes(&grown)));
    assert_eq!(complete_native.segments, 2);
    assert_eq!(complete_native.trailing_bytes_held, 11);

    let exported = temp.path().join("one.vibe-out");
    write_export(&exported, SESSION);
    finished_native(&exported.join("session"));
    let (complete, copied) = project(&exported);
    assert_eq!(complete.coverage(), "complete");
    assert_eq!(complete.stats.projections_emitted, 6);
    assert_eq!(complete.journal_records, 4);
    assert_eq!(grown.facts(), copied.facts());
    assert_eq!(copied.facts(), project(&exported).1.facts());

    let replies: Vec<Id> = find!(response: Id, pattern!(&copied, [{
        ?response @ block::responds_to: _?call, block::holds: _?result_fact
    }, {
        _?call @ block::holds: _?call_fact
    }, {
        _?call_fact @ content_fact::modality: content_fact::modality::TOOL_CALL
    }, {
        _?result_fact @ content_fact::modality: content_fact::modality::TOOL_RESULT
    }]))
    .collect();
    assert_eq!(replies.len(), 1);
    assert!(exists!(pattern!(&copied, [{
        _?receipt @ source_projection::source_locator: format!("session/{SESSION}"),
        source_projection::source_namespace: SOURCE_VIBE,
    }])));
}

#[test]
fn failed_call_needs_durable_evidence_of_delivery() {
    let temp = tempfile::tempdir().unwrap();
    let native = temp.path().join(SESSION);
    write_current(&native, SESSION, 0);
    segment(&native, 1, &[initial(), failure("first-action")], b"");
    let (missing, facts) = project(&native);
    assert_eq!(missing.coverage(), "missing");
    assert_eq!(missing.failed_calls, 1);
    assert!(facts.facts().is_empty());

    let first = json!({
        "type":"message", "id":"assistant-first-action", "role":"assistant",
        "source":"harness", "generationStatus":"completed",
        "outcome":{"type":"discarded","cause":"failure"},
        "content":[{"type":"text","text":"emitted fragment"}],
    });
    let discarded = (
        "projection_delta",
        json!({
            "delta": [
                {"op":"append_entry", "entry": first},
                {"op":"replace_entry", "id":"assistant-first-action", "entry": first},
            ]
        }),
    );
    let reasoning = (
        "projection_delta",
        json!({
            "delta": [{"op":"append_entry", "entry": {
                "type":"reasoning", "id":"reasoning-first-action-0",
                "generationStatus":"completed",
                "outcome":{"type":"discarded","cause":"failure"},
                "text":"retained thought"
            }}]
        }),
    );
    segment(
        &native,
        1,
        &[
            initial(),
            failure("first-action"),
            discarded.clone(),
            discarded,
            reasoning,
        ],
        b"",
    );
    let (partial, facts) = project(&native);
    assert_eq!(partial.coverage(), "partial");
    assert_eq!(partial.streamed_calls, 1);
    assert_eq!(nodes(&facts).len(), 4);
    assert!(exists!(pattern!(&facts, [{
        _?receipt @ source_projection::raw_outcome: "discarded"
    }])));
}

#[test]
fn catalog_definitions_are_delivered_once_per_change() {
    let temp = tempfile::tempdir().unwrap();
    let native = temp.path().join(SESSION);
    write_current(&native, SESSION, 0);
    let mut initial = initial();
    initial.1["request"]["model_input"]["tool_catalog"] = json!({
        "type":"replace", "revision":2,
        "tools":[{"name":"lookup","description":"Find synthetic text","parameters":{"type":"object"}}]
    });
    let mut next = second_intent();
    next.1["request"]["model_input"]["tool_catalog"] = json!({"type":"keep","revision":2});
    segment(
        &native,
        1,
        &[initial, first_completion(), next, answer()],
        b"",
    );
    let (summary, facts) = project(&native);
    assert_eq!(summary.catalog_replacements, 1);
    assert_eq!(nodes(&facts).len(), 7);
    assert!(exists!(pattern!(&facts, [{
        _?fact @ content_fact::modality: content_fact::modality::EVENT,
        content_fact::direction: content_fact::direction::IN,
        content_fact::payload: r#"{"tool_catalog":[{"description":"Find synthetic text","name":"lookup","parameters":{"type":"object"}}]}"#,
    }])));
}

#[test]
fn exact_run_selection_ignores_a_predecessors_export_and_reports_retention_loss() {
    let temp = tempfile::tempdir().unwrap();
    let output = temp.path().join("one.vibe-out");
    let home = temp.path().join("one.vibe");
    let native = home.join(NATIVE_SESSIONS).join(SESSION);
    write_export(&output, PREDECESSOR);
    assert_eq!(run_source(&output, &home, SESSION).unwrap(), None);
    write_current(&native, SESSION, 5);
    segment(&native, 6, &[answer()], b"");
    assert_eq!(
        run_source(&output, &home, SESSION).unwrap(),
        Some(native.clone())
    );
    let (missing, fragment) = project(&native);
    assert_eq!(missing.coverage(), "missing");
    assert_eq!(missing.missing_sequence, Some(1));
    assert!(fragment.facts().is_empty());
    write_export(&output, SESSION);
    finished_native(&output.join("session"));
    assert_eq!(run_source(&output, &home, SESSION).unwrap(), Some(output));
}

#[test]
fn damaged_full_record_has_no_publication() {
    let temp = tempfile::tempdir().unwrap();
    let native = temp.path().join(SESSION);
    write_current(&native, SESSION, 0);
    let path = segment(&native, 1, &[initial(), first_completion()], b"");
    let data = fs::read_to_string(&path).unwrap();
    fs::write(path, data.replace("a thought", "a different thought")).unwrap();
    let error = project_path(&native, |_| panic!("damaged source reached the sink")).unwrap_err();
    assert!(format!("{error:#}").contains("digest chain mismatch"));
}

#[test]
#[ignore = "requires exact local exported and native Vibe directories"]
fn held_vibe_runs_agree_in_memory() {
    let export = std::env::var_os("VIBE_VALIDATION_EXPORT").expect("supply exported run directory");
    let native = std::env::var_os("VIBE_VALIDATION_NATIVE").expect("supply native UUID directory");
    let (exported, export_graph) = project(Path::new(&export));
    let (native, native_graph) = project(Path::new(&native));
    assert_eq!(exported.coverage(), "complete");
    assert!(native.session_id == exported.session_id);
    assert!(native_graph.facts() == export_graph.facts());
    assert!(export_graph.facts() == project(Path::new(&export)).1.facts());
    assert_eq!(exported.stats.unresolved_tool_results, 0);
    println!(
        "segments={} journal_records={} completed_calls={} catalog_replacements={} nodes={} unresolved_tool_results={}",
        exported.segments,
        exported.journal_records,
        exported.completed_calls,
        exported.catalog_replacements,
        exported.stats.projections_emitted,
        exported.stats.unresolved_tool_results,
    );
}
