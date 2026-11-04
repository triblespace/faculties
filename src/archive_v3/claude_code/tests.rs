//! Tests of the v3 Claude Code importer, on synthetic transcripts only.
//!
//! Every fixture is gated: the importer's fragment must pass the gate
//! against the same source bytes. Each case then checks the shape it is
//! about (a gloss, a gap, a copy, an annotation) directly.

use std::collections::BTreeMap;

use anybytes::View;
use serde_json::{json, Value};
use triblespace::core::blob::MemoryBlobStoreSnapshot;
use triblespace::core::repo::BlobStoreGet;
use triblespace::prelude::blobencodings::UTF8String;
use triblespace::prelude::inlineencodings::Handle;

use super::*;
use crate::archive_v3::gate::{self, Kind, Report};
use crate::schemas::archive_v3 as v3;

const PNG: &str = "iVBORw0KGgo=";
const PDF: &str = "JVBERi0xLjQK";
const SIGNATURE: &str = "c2lnbmVkIHRoaW5raW5n";

struct Imported {
    fragment: Fragment,
    blobs: MemoryBlobStoreSnapshot,
}

fn jsonl(records: &[Value]) -> String {
    records
        .iter()
        .map(|record| serde_json::to_string(record).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

fn import_ok(source: &str) -> Imported {
    let mut fragment = import(source.as_bytes()).unwrap_or_else(|failure| panic!("{failure}"));
    let blobs = fragment.blobs_mut().snapshot().unwrap();
    Imported { fragment, blobs }
}

fn gate(source: &str, imported: &Imported) -> Report {
    gate::check(
        &TABLE,
        source.as_bytes(),
        imported.fragment.facts(),
        &imported.blobs,
    )
}

/// Import and gate; both must pass.
fn passes(source: &str) -> Imported {
    let imported = import_ok(source);
    let report = gate(source, &imported);
    assert!(report.passed(), "{report}");
    imported
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

fn thinking(readable: &str) -> Value {
    json!({"type": "thinking", "thinking": readable, "signature": SIGNATURE})
}

fn call(id: &str, name: &str, input: Value) -> Value {
    json!({"type": "tool_use", "id": id, "name": name, "input": input, "caller": {"type": "direct"}})
}

fn result(call: &str, content: Value) -> Value {
    json!([{"type": "tool_result", "tool_use_id": call, "content": content, "is_error": false}])
}

fn text_of(imported: &Imported, handle: Inline<Handle<UTF8String>>) -> String {
    imported
        .blobs
        .get::<View<str>, UTF8String>(handle)
        .unwrap()
        .as_ref()
        .to_owned()
}

/// The nodes that hold a fact with this payload.
fn nodes_saying(imported: &Imported, text: &str) -> Vec<Id> {
    let facts = imported.fragment.facts();
    let held: Vec<(Id, Inline<Handle<UTF8String>>)> = find!(
        (node: Id, payload: Inline<Handle<UTF8String>>),
        pattern!(facts, [
            { ?node @ v3::node::holds: _?fact },
            { _?fact @ v3::fact::payload: ?payload },
        ])
    )
    .collect();
    held.into_iter()
        .filter(|(_, payload)| text_of(imported, *payload) == text)
        .map(|(node, _)| node)
        .collect()
}

fn previous(imported: &Imported, node: Id) -> Option<Id> {
    let facts = imported.fragment.facts();
    let found: Vec<Id> = find!(
        previous: Id,
        pattern!(facts, [{ node @ v3::node::previous: ?previous }])
    )
    .collect();
    assert!(found.len() <= 1);
    found.first().copied()
}

/// The modalities and gap kinds along the window before `node`, root first.
fn window_before(imported: &Imported, node: Id) -> Vec<(Id, Option<Id>)> {
    let facts = imported.fragment.facts();
    let mut out = Vec::new();
    let mut cursor = previous(imported, node);
    while let Some(at) = cursor {
        let held: Vec<Id> = find!(
            modality: Id,
            pattern!(facts, [
                { at @ v3::node::holds: _?fact },
                { _?fact @ v3::fact::modality: ?modality },
            ])
        )
        .collect();
        let absent: Vec<Id> = find!(
            absent: Id,
            pattern!(facts, [
                { at @ v3::node::holds: _?fact },
                { _?fact @ v3::fact::absent: ?absent },
            ])
        )
        .collect();
        out.push((held[0], absent.first().copied()));
        cursor = previous(imported, at);
    }
    out.reverse();
    out
}

fn glosses(imported: &Imported, category: Id) -> Vec<String> {
    let facts = imported.fragment.facts();
    let found: Vec<Inline<Handle<UTF8String>>> = find!(
        text: Inline<Handle<UTF8String>>,
        pattern!(facts, [{ _?gloss @ v3::gloss::category: category, v3::gloss::text: ?text }])
    )
    .collect();
    found.into_iter().map(|t| text_of(imported, t)).collect()
}

macro_rules! annotated {
    ($imported:expr, $attribute:path) => {{
        let facts = $imported.fragment.facts();
        let found: Vec<(Id, Id)> = find!(
            (node: Id, value: Id),
            pattern!(facts, [{ ?node @ $attribute: ?value }])
        )
        .collect();
        found
    }};
}

fn ends_of(imported: &Imported, node: Id) -> Vec<Id> {
    let facts = imported.fragment.facts();
    find!(
        term: Id,
        pattern!(facts, [{ node @ v3::node::ends: ?term }])
    )
    .collect()
}

fn refused(source: &str) -> Failure {
    match import(source.as_bytes()) {
        Ok(_) => panic!("the importer took a unit it must refuse"),
        Err(failure) => failure,
    }
}

fn walk_rows(source: &str) -> Vec<(String, Kind)> {
    gate::walk::walk(&TABLE, source.as_bytes())
        .rows
        .into_iter()
        .map(|(path, kind, _)| (path, kind))
        .collect()
}

// ---------------------------------------------------------------------------

/// The task-2 witness: every kind of tool_result item, imported by v3,
/// passes where main lost the tool_reference and the document.
#[test]
fn every_tool_result_item_passes_the_gate() {
    use crate::archive_v3::gate::tests::{
        claude_code_source, RESULT_DOCUMENT, RESULT_IMAGE, RESULT_REFERENCE, RESULT_TEXT,
    };
    let source =
        claude_code_source(&[RESULT_TEXT, RESULT_IMAGE, RESULT_REFERENCE, RESULT_DOCUMENT]);
    let imported = passes(&source);
    // The result head answers the call.
    assert_eq!(annotated!(imported, v3::node::responds_to).len(), 1);
    // Every window starts with what the system section held, unrecorded.
    let leaf = nodes_saying(&imported, "Thanks.")[0];
    let window = window_before(&imported, leaf);
    assert_eq!(
        window[..2],
        [
            (v3::fact::modality::SYSTEM, Some(v3::fact::absent::BODY)),
            (
                v3::fact::modality::TOOL_DEFINITION,
                Some(v3::fact::absent::SPAN)
            ),
        ]
    );
}

#[test]
fn text_images_documents_and_calls_pass() {
    let source = jsonl(&[
        user(
            "u1",
            None,
            0,
            json!([
                said("Look at these."),
                {"type": "image", "source": {"type": "base64", "media_type": "image/PNG", "data": PNG}},
                {"type": "document", "source": {"type": "base64", "media_type": "application/pdf", "data": PDF}},
            ]),
        ),
        assistant(
            "a1",
            "u1",
            1,
            "m1",
            said("I see a picture and a file."),
            json!(null),
        ),
        assistant(
            "a2",
            "a1",
            2,
            "m1",
            call("t1", "Read", json!({"file_path": "/x", "limit": 1.50})),
            json!("tool_use"),
        ),
        user("u2", Some("a2"), 3, result("t1", json!("  \n"))),
        assistant("a3", "u2", 4, "m2", said("Done."), json!("end_turn")),
    ]);
    let imported = passes(&source);
    let done = nodes_saying(&imported, "Done.")[0];
    assert_eq!(ends_of(&imported, done), vec![v3::node::ends::END_TURN]);
    // Whitespace-only result text is kept.
    assert_eq!(nodes_saying(&imported, "  \n").len(), 1);
}

#[test]
fn sealed_thinking_is_bytes_and_its_readable_text_a_gloss() {
    let source = jsonl(&[
        user("u1", None, 0, json!("Think first.")),
        assistant(
            "a1",
            "u1",
            1,
            "m1",
            thinking("A summary of my thinking."),
            json!(null),
        ),
        assistant("a2", "a1", 2, "m1", thinking(""), json!(null)),
        assistant("a3", "a2", 3, "m1", said("Thought."), json!("end_turn")),
    ]);
    let imported = passes(&source);
    assert_eq!(
        glosses(&imported, v3::gloss::category::REASONING_SUMMARY),
        vec!["A summary of my thinking."]
    );
    // The readable text is never a fact.
    assert!(nodes_saying(&imported, "A summary of my thinking.").is_empty());
    let answer = nodes_saying(&imported, "Thought.")[0];
    let window = window_before(&imported, answer);
    let sealed = window
        .iter()
        .filter(|(m, _)| *m == v3::fact::modality::THINKING)
        .count();
    assert_eq!(sealed, 2);

    // A model the gloss table does not name fails.
    let unknown = source.replace("claude-opus-4-6", "claude-3-7-sonnet");
    assert!(import(unknown.as_bytes()).is_err());
    // Only the empty drop names the path for that model, so readable text
    // there is a finding.
    assert!(walk_rows(&unknown).contains(&(
        "{assistant}.message{message}.content[]{thinking}.thinking".to_owned(),
        Kind::Empty
    )));
}

#[test]
fn harness_text_in_her_slot_is_voiced_input() {
    let mut error = assistant(
        "a1",
        "u1",
        1,
        "m1",
        said("API Error: synthetic"),
        json!("stop_sequence"),
    );
    error["message"]["model"] = json!("<synthetic>");
    error["message"]["stop_sequence"] = json!("");
    error["isApiErrorMessage"] = json!(true);
    let source = jsonl(&[user("u1", None, 0, json!("Hello?")), error]);
    let imported = passes(&source);
    let node = nodes_saying(&imported, "API Error: synthetic")[0];
    let facts = imported.fragment.facts();
    let (direction, form): (Id, Id) = find!(
        (direction: Id, form: Id),
        pattern!(facts, [
            { node @ v3::node::holds: _?fact },
            { _?fact @ v3::fact::direction: ?direction, v3::fact::form: ?form },
        ])
    )
    .next()
    .unwrap();
    assert_eq!(direction, v3::fact::direction::IN);
    assert_eq!(form, v3::fact::form::VOICED);
    assert_eq!(ends_of(&imported, node), vec![v3::node::ends::ERROR]);
}

#[test]
fn interruptions_end_the_message_they_interrupt() {
    let mut stop = user(
        "u2",
        Some("a1"),
        2,
        json!([said("[Request interrupted by user]")]),
    );
    stop["interruptedMessageId"] = json!("m1");
    let source = jsonl(&[
        user("u1", None, 0, json!("Go.")),
        assistant("a1", "u1", 1, "m1", said("Going"), json!(null)),
        stop,
    ]);
    let imported = passes(&source);
    let going = nodes_saying(&imported, "Going")[0];
    assert_eq!(ends_of(&imported, going), vec![v3::node::ends::INTERRUPTED]);
    // The notice she was shown is an `in` fact of its own.
    assert_eq!(
        nodes_saying(&imported, "[Request interrupted by user]").len(),
        1
    );
}

#[test]
fn transcript_only_records_and_tool_originals_are_dropped() {
    let mut hidden = user("u2", Some("u1"), 1, json!("Shown in the transcript only."));
    hidden["isVisibleInTranscriptOnly"] = json!(true);
    let mut answer = user(
        "u3",
        Some("a1"),
        3,
        result("t1", json!([said("one result")])),
    );
    answer["toolUseResult"] = json!({"stdout": "the full original", "originalFile": "x"});
    let source = jsonl(&[
        user("u1", None, 0, json!("Start.")),
        hidden,
        assistant(
            "a1",
            "u2",
            2,
            "m1",
            call("t1", "Bash", json!({"command": "true"})),
            json!("tool_use"),
        ),
        answer,
    ]);
    let imported = passes(&source);
    assert!(nodes_saying(&imported, "Shown in the transcript only.").is_empty());
    assert!(nodes_saying(&imported, "the full original").is_empty());
}

#[test]
fn rendered_attachments_are_what_she_saw() {
    let attachment = json!({
        "type": "attachment", "sessionId": "s-1", "uuid": "x1", "parentUuid": "u1",
        "timestamp": stamp(1), "cwd": "/synthetic", "version": "0.0.0",
        "attachment": {"type": "date", "date": "2026-03-01"},
        "rendered": [{"content": "<system-reminder>Today is 2026-03-01.</system-reminder>"}],
        "renderedRole": "system",
    });
    let human = json!({
        "type": "attachment", "sessionId": "s-1", "uuid": "x2", "parentUuid": "x1",
        "timestamp": stamp(2), "attachment": {"type": "queued_command", "prompt": "later"},
        "rendered": [{"content": "A queued note."}],
        "renderedInHumanTurn": [{"content": "later"}],
        "renderedRole": "user",
    });
    let source = jsonl(&[
        user("u1", None, 0, json!("Hi.")),
        attachment,
        human,
        assistant("a1", "x2", 3, "m1", said("Hello."), json!("end_turn")),
    ]);
    let imported = passes(&source);
    let hello = nodes_saying(&imported, "Hello.")[0];
    let window = window_before(&imported, hello);
    assert!(window.contains(&(v3::fact::modality::SYSTEM, None)));

    // An attachment recorded before rendered[] is open: it fails.
    let old = json!({
        "type": "attachment", "sessionId": "s-1", "uuid": "x1", "parentUuid": "u1",
        "timestamp": stamp(1), "attachment": {"type": "date", "date": "2026-03-01"},
    });
    let source = jsonl(&[user("u1", None, 0, json!("Hi.")), old]);
    assert_eq!(refused(&source).path, "{attachment}.attachment");
    assert!(walk_rows(&source).contains(&("{attachment}.attachment".to_owned(), Kind::Open)));
}

fn shape(second: u32, system: &str, tools: &[(&str, &str)]) -> Value {
    let tools: Vec<Value> = tools
        .iter()
        .map(|(name, description)| {
            json!({"name": name, "description": description, "defer_loading": false,
                   "eager_input_streaming": true,
                   "input_schema": {"type": "object", "properties": {"q": {"type": "string"}}}})
        })
        .collect();
    json!({
        "type": "api-request-shape", "sessionId": "s-1", "timestamp": stamp(second),
        "version": "0.0.0",
        "shape": {"system": [{"type": "text", "text": system}], "tools": tools}
    })
}

#[test]
fn recorded_system_prompts_and_tools_open_the_window() {
    let source = jsonl(&[
        shape(
            0,
            "You are synthetic.",
            &[("Search", "Search the web."), ("Read", "Read a file.")],
        ),
        user("u1", None, 1, json!("Search for it.")),
        assistant("a1", "u1", 2, "m1", said("Searching."), json!("end_turn")),
        // A new shape: later windows are rebuilt on it, with copies.
        shape(
            3,
            "You are synthetic, again.",
            &[("Search", "Search the web.")],
        ),
        user("u2", Some("a1"), 4, json!("Again.")),
        assistant("a2", "u2", 5, "m2", said("Again."), json!("end_turn")),
    ]);
    let imported = passes(&source);
    let first = nodes_saying(&imported, "Searching.")[0];
    let window = window_before(&imported, first);
    assert_eq!(
        window,
        vec![
            (v3::fact::modality::SYSTEM, None),
            (v3::fact::modality::TOOL_DEFINITION, None),
            (v3::fact::modality::TOOL_DEFINITION, None),
            (v3::fact::modality::TEXT, None),
        ]
    );
    let again = nodes_saying(&imported, "Again.");
    assert_eq!(again.len(), 2, "the prompt and her answer");
    // The prompt and her first answer are re-presented after the new shape.
    assert_eq!(nodes_saying(&imported, "Search for it.").len(), 2);
    assert_eq!(annotated!(imported, v3::node::recalls).len(), 2);
}

#[test]
fn compaction_starts_a_root_that_continues_and_recalls() {
    let boundary = json!({
        "type": "system", "subtype": "compact_boundary", "uuid": "b1", "parentUuid": null,
        "logicalParentUuid": "a2", "sessionId": "s-1", "timestamp": stamp(4),
        "content": "Conversation compacted", "level": "info", "isMeta": false,
        "compactMetadata": {"trigger": "auto", "preTokens": 100,
            "preservedSegment": {"headUuid": "a2", "tailUuid": "a2", "anchorUuid": "c1"},
            "preservedMessages": {"uuids": ["a2"]}}
    });
    let mut summary = user("c1", Some("b1"), 5, json!("Summary of what came before."));
    summary["isCompactSummary"] = json!(true);
    let source = jsonl(&[
        user("u1", None, 0, json!("First.")),
        assistant("a1", "u1", 1, "m1", said("One."), json!("end_turn")),
        user("u2", Some("a1"), 2, json!("Second.")),
        assistant("a2", "u2", 3, "m2", said("Two."), json!("end_turn")),
        boundary,
        summary,
        user("u3", Some("c1"), 6, json!("Third.")),
        assistant("a3", "u3", 7, "m3", said("Three."), json!("end_turn")),
    ]);
    let imported = passes(&source);
    let summary = nodes_saying(&imported, "Summary of what came before.")[0];
    let two = nodes_saying(&imported, "Two.");
    assert_eq!(two.len(), 2, "the original and its copy after the summary");
    let continues = annotated!(imported, v3::node::continues);
    assert_eq!(continues.len(), 1);
    assert_eq!(continues[0].0, summary);
    assert!(
        two.contains(&continues[0].1),
        "it continues from the old tail"
    );
    let recalls = annotated!(imported, v3::node::recalls);
    assert_eq!(recalls.len(), 1);
    assert!(two.contains(&recalls[0].0) && two.contains(&recalls[0].1));
    // The new window holds the summary, then the preserved answer.
    let three = nodes_saying(&imported, "Three.")[0];
    let window = window_before(&imported, three);
    assert_eq!(window.len(), 2 + 3, "system section, summary, copy, prompt");
}

#[test]
fn dropped_thinking_leaves_her_window() {
    let mut second = assistant(
        "a3",
        "u2",
        4,
        "m2",
        said("Second answer."),
        json!("end_turn"),
    );
    second["message"]["input_transformations"] = json!([
        {"type": "thinking_dropped", "path": "messages.1.content.0", "reason": "prefix_binding_mismatch"}
    ]);
    let drop = json!({
        "type": "attachment", "sessionId": "s-1", "uuid": "x1", "parentUuid": "a3",
        "timestamp": stamp(5),
        "attachment": {"type": "thinking_drop", "requestId": "req-m2", "thinkingBlocksSent": 1,
            "newlyDropped": {"blockCount": 1, "reason": "prefix_mismatch"}, "blockHashes": ["h"]}
    });
    let source = jsonl(&[
        user("u1", None, 0, json!("First.")),
        assistant("a1", "u1", 1, "m1", thinking("first thoughts"), json!(null)),
        assistant(
            "a2",
            "a1",
            2,
            "m1",
            said("First answer."),
            json!("end_turn"),
        ),
        user("u2", Some("a2"), 3, json!("Second.")),
        second,
        drop,
    ]);
    let imported = passes(&source);
    let answer = nodes_saying(&imported, "Second answer.")[0];
    let window = window_before(&imported, answer);
    assert!(
        !window
            .iter()
            .any(|(m, _)| *m == v3::fact::modality::THINKING),
        "the dropped block is not in the window she was sampled from"
    );
    // The first answer and the prompt after it are copies.
    assert_eq!(annotated!(imported, v3::node::recalls).len(), 2);

    // A drop the message does not mark fails.
    let unmarked = source.replace("\"blockCount\":1", "\"blockCount\":2");
    assert!(import(unmarked.as_bytes()).is_err());
    // A path that is not a thinking block fails.
    let wrong = source.replace("messages.1.content.0", "messages.1.content.1");
    assert!(import(wrong.as_bytes()).is_err());
}

#[test]
fn cleared_results_are_body_gaps_in_her_window() {
    let mut later = assistant("a3", "u3", 5, "m3", said("Later."), json!("end_turn"));
    later["message"]["context_management"] = json!({"applied_edits": [
        {"type": "clear_tool_uses_20250919", "cleared_tool_uses": 1, "cleared_input_tokens": 9}
    ]});
    let source = jsonl(&[
        user("u1", None, 0, json!("Run it.")),
        assistant(
            "a1",
            "u1",
            1,
            "m1",
            call("t1", "Bash", json!({"command": "ls"})),
            json!("tool_use"),
        ),
        user(
            "u2",
            Some("a1"),
            2,
            result("t1", json!([said("a long listing")])),
        ),
        assistant("a2", "u2", 3, "m2", said("Listed."), json!("end_turn")),
        user("u3", Some("a2"), 4, json!("And now?")),
        later,
    ]);
    let imported = passes(&source);
    let node = nodes_saying(&imported, "Later.")[0];
    let window = window_before(&imported, node);
    assert!(window.contains(&(v3::fact::modality::TEXT, Some(v3::fact::absent::BODY))));
    assert!(window.contains(&(v3::fact::modality::TOOL_RESULT, None)));
    let listing = nodes_saying(&imported, "a long listing");
    assert_eq!(
        listing.len(),
        1,
        "the cleared text is only in the earlier window"
    );
}

#[test]
fn titles_and_recaps_are_glosses() {
    let title = json!({"type": "ai-title", "sessionId": "s-1", "aiTitle": "A synthetic chat"});
    let recap = json!({
        "type": "system", "subtype": "away_summary", "uuid": "r1", "parentUuid": "a1",
        "sessionId": "s-1", "timestamp": stamp(3), "content": "While away: one answer.",
        "isMeta": false
    });
    let source = jsonl(&[
        user("u1", None, 0, json!("Hi.")),
        assistant("a1", "u1", 1, "m1", said("Hello."), json!("end_turn")),
        title,
        recap,
    ]);
    let imported = passes(&source);
    assert_eq!(
        glosses(&imported, v3::gloss::category::TITLE),
        vec!["A synthetic chat"]
    );
    assert_eq!(
        glosses(&imported, v3::gloss::category::RECAP),
        vec!["While away: one answer."]
    );
}

#[test]
fn wire_inputs_must_mirror_the_call() {
    let mut a1 = assistant(
        "a1",
        "u1",
        1,
        "m1",
        call("t1", "Bash", json!({"command": "ls"})),
        json!("tool_use"),
    );
    a1["wireToolInputs"] = json!({"t1": {"command": "ls"}});
    let source = jsonl(&[user("u1", None, 0, json!("Run.")), a1]);
    passes(&source);

    let differs = source.replace(
        "\"wireToolInputs\":{\"t1\":{\"command\":\"ls\"}}",
        "\"wireToolInputs\":{\"t1\":{\"command\":\"ls -l\"}}",
    );
    assert_ne!(differs, source);
    assert_eq!(refused(&differs).path, "{assistant}.wireToolInputs");
    assert!(walk_rows(&differs).contains(&("{assistant}.wireToolInputs".to_owned(), Kind::Mirror)));
}

#[test]
fn unknown_and_open_fields_fail_the_unit() {
    let mut record = user("u1", None, 0, json!("Hi."));
    record["somethingNew"] = json!(1);
    let source = jsonl(&[record]);
    assert_eq!(refused(&source).path, "{user}.somethingNew");
    assert!(walk_rows(&source).contains(&("{user}.somethingNew".to_owned(), Kind::Unknown)));

    let mut effort = assistant("a1", "u1", 1, "m1", said("Hello."), json!("end_turn"));
    effort["effort"] = json!("high");
    let source = jsonl(&[user("u1", None, 0, json!("Hi.")), effort]);
    assert_eq!(refused(&source).path, "{assistant}.effort");

    let source = jsonl(&[json!({"type": "brand-new-kind", "sessionId": "s-1"})]);
    assert!(import(source.as_bytes()).is_err());
}

#[test]
fn a_record_written_twice_is_one_record() {
    let first = user("u1", None, 0, json!("Once."));
    let mut again = first.clone();
    again["cwd"] = json!("/elsewhere");
    again["promptId"] = json!("p-2");
    let answer = assistant("a1", "u1", 1, "m1", said("Heard."), json!("end_turn"));
    let source = jsonl(&[first.clone(), again, answer.clone()]);
    let imported = passes(&source);
    assert_eq!(nodes_saying(&imported, "Once.").len(), 1);

    let mut other = first.clone();
    other["message"]["content"] = json!("Twice.");
    let source = jsonl(&[first, other, answer]);
    assert_eq!(refused(&source).path, "{user}.uuid");
}

#[test]
fn the_gloss_table_matches_the_table_rows() {
    let rows: Vec<&str> = TABLE
        .entries
        .iter()
        .filter(|entry| entry.path == "{assistant}.message{message}.content[]{thinking}.thinking")
        .flat_map(|entry| entry.when.iter())
        .filter_map(|cond| match cond {
            crate::archive_v3::fields::Cond::Prefix(_, prefix) => Some(*prefix),
            _ => None,
        })
        .collect();
    assert_eq!(rows, SUMMARIZED_THINKING);
}

// ---------------------------------------------------------------------------
// The real sample. Prints only paths, kinds and counts.

#[test]
#[ignore = "reads real transcripts named by AV3_SAMPLE; prints no values"]
fn real_sample() {
    let list = std::env::var("AV3_SAMPLE").expect("AV3_SAMPLE names a file list");
    let files: Vec<String> = std::fs::read_to_string(list)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    let mut passed = 0;
    let mut import_failed = 0;
    let mut gate_failed = 0;
    let mut first_failures: BTreeMap<(String, &'static str), usize> = BTreeMap::new();
    let mut walk_failures: BTreeMap<(String, String), usize> = BTreeMap::new();
    let mut gate_failures: BTreeMap<(String, String), usize> = BTreeMap::new();
    for file in &files {
        let Ok(bytes) = std::fs::read(file) else {
            continue;
        };
        let walked = gate::walk::walk(&TABLE, &bytes);
        let mut seen = std::collections::BTreeSet::new();
        for (path, kind, _) in &walked.rows {
            seen.insert((path.clone(), kind.to_string()));
        }
        for row in seen {
            *walk_failures.entry(row).or_default() += 1;
        }
        drop(walked);
        match import(&bytes) {
            Err(failure) => {
                import_failed += 1;
                *first_failures
                    .entry((failure.path, failure.reason))
                    .or_default() += 1;
            }
            Ok(mut fragment) => {
                let blobs = fragment.blobs_mut().snapshot().unwrap();
                let report = gate::check(&TABLE, &bytes, fragment.facts(), &blobs);
                if report.passed() {
                    passed += 1;
                } else {
                    gate_failed += 1;
                    for row in report.rows {
                        *gate_failures
                            .entry((row.path, row.kind.to_string()))
                            .or_default() += 1;
                    }
                }
            }
        }
    }
    println!(
        "files {} passed {} import-failed {} gate-failed {}",
        files.len(),
        passed,
        import_failed,
        gate_failed
    );
    println!("-- importer's first failure: files, path, reason");
    for ((path, reason), count) in &first_failures {
        println!("{count}\t{path}\t{reason}");
    }
    println!("-- walker rows: files, path, kind");
    for ((path, kind), count) in &walk_failures {
        println!("{count}\t{path}\t{kind}");
    }
    println!("-- gate rows of imported files: files, path, kind");
    for ((path, kind), count) in &gate_failures {
        println!("{count}\t{path}\t{kind}");
    }
}
