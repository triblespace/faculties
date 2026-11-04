//! Tests of the v3 Claude Code importer, on synthetic transcripts only.
//!
//! Every fixture is gated: the importer's fragment must pass the gate
//! against the same source bytes. Each case then checks the shape it is
//! about (a gloss, a gap, a copy, an annotation) directly.

use std::collections::{BTreeMap, BTreeSet};

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
/// The modality `window_before` reports for a span gap of unknown kind.
const UNKNOWN: Id = triblespace::macros::id_hex!("00000000000000000000000000000001");

/// The system section before anything is recorded: the settings header
/// gap, the system prompt gap and the tool definitions span.
fn unrecorded_section() -> [(Id, Option<Id>); 3] {
    [
        (v3::fact::modality::SYSTEM, Some(v3::fact::absent::BODY)),
        (v3::fact::modality::SYSTEM, Some(v3::fact::absent::BODY)),
        (
            v3::fact::modality::TOOL_DEFINITION,
            Some(v3::fact::absent::SPAN),
        ),
    ]
}

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

/// The node with this payload that a record held: the one a witness
/// projects to.
fn witnessed_saying(imported: &Imported, text: &str) -> Id {
    let facts = imported.fragment.facts();
    let held: Vec<Id> = nodes_saying(imported, text)
        .into_iter()
        .filter(|&node| {
            exists!(
                (),
                pattern!(facts, [{ _?witness @ v3::witness::projects_to: node }])
            )
        })
        .collect();
    assert_eq!(held.len(), 1, "{text}");
    held[0]
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

/// The modalities and gap kinds along the window before `node`, root first;
/// a span gap of unknown kind has the nil modality.
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
        out.push((
            held.first().copied().unwrap_or(UNKNOWN),
            absent.first().copied(),
        ));
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
    let leaf = witnessed_saying(&imported, "Thanks.");
    let window = window_before(&imported, leaf);
    assert_eq!(window[..3], unrecorded_section());
    // The tool_reference loads a definition no source records: a body gap
    // that carries the tool's name (I9).
    let facts = imported.fragment.facts();
    let references: Vec<(Id, Inline<Handle<UTF8String>>)> = find!(
        (absent: Id, name: Inline<Handle<UTF8String>>),
        pattern!(facts, [{ _?fact @
            v3::fact::modality: v3::fact::modality::TOOL_DEFINITION,
            v3::fact::absent: ?absent,
            v3::fact::tool_name: ?name,
        }])
    )
    .collect();
    assert_eq!(references.len(), 1);
    assert_eq!(references[0].0, v3::fact::absent::BODY);
    assert_eq!(text_of(&imported, references[0].1), "WebSearch");
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
    let done = witnessed_saying(&imported, "Done.");
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
    let answer = witnessed_saying(&imported, "Thought.");
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
    let node = witnessed_saying(&imported, "API Error: synthetic");
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
    let going = witnessed_saying(&imported, "Going");
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
    let hello = witnessed_saying(&imported, "Hello.");
    let window = window_before(&imported, hello);
    assert!(window.contains(&(v3::fact::modality::SYSTEM, None)));

    // Beside its rendering, every field of the data is named (I8): one the
    // table does not name fails.
    let mut stray = json!({
        "type": "attachment", "sessionId": "s-1", "uuid": "x1", "parentUuid": "u1",
        "timestamp": stamp(1), "attachment": {"type": "date", "date": "2026-03-01", "zone": "x"},
        "rendered": [{"content": "Today."}], "renderedRole": "system",
    });
    let source = jsonl(&[user("u1", None, 0, json!("Hi.")), stray.clone()]);
    assert_eq!(refused(&source).path, "{attachment}.attachment{date}.zone");
    assert!(walk_rows(&source).contains(&(
        "{attachment}.attachment{date}.zone".to_owned(),
        Kind::Unknown
    )));
    // And so does a kind it does not name.
    stray["attachment"] = json!({"type": "brand_new_kind", "x": 1});
    let source = jsonl(&[user("u1", None, 0, json!("Hi.")), stray]);
    assert!(import(source.as_bytes()).is_err());
    // A queued prompt with parts is open: an image's place is unverified.
    let parts = json!({
        "type": "attachment", "sessionId": "s-1", "uuid": "x1", "parentUuid": "u1",
        "timestamp": stamp(1),
        "attachment": {"type": "queued_command", "commandMode": "prompt",
            "prompt": [{"type": "text", "text": "look"},
                       {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": PNG}}]},
        "rendered": [{"content": "look"}], "renderedRole": "user",
    });
    let source = jsonl(&[user("u1", None, 0, json!("Hi.")), parts]);
    assert_eq!(
        refused(&source).path,
        "{attachment}.attachment{queued_command}.prompt[]"
    );
}

/// S3: an attachment recorded before renderings were keeps its data as
/// `in` event facts, and its nodes say the rendering is unrecorded.
#[test]
fn unrecorded_attachments_keep_their_data() {
    let old = json!({
        "type": "attachment", "sessionId": "s-1", "uuid": "x1", "parentUuid": "u1",
        "timestamp": stamp(1), "cwd": "/synthetic",
        "attachment": {"type": "hook_success", "hookName": "SessionStart", "hookEvent": "SessionStart",
            "toolUseID": "toolu_01AbCdEfGhIjKlMnOp", "command": "echo hi", "content": "",
            "stdout": "hi\n", "stderr": "", "exitCode": 0, "durationMs": 12},
    });
    let copies = json!({
        "type": "attachment", "sessionId": "s-1", "uuid": "x2", "parentUuid": "x1",
        "timestamp": stamp(2),
        "attachment": {"type": "deferred_tools_record", "entries": [],
            "toolInputCopies": [{"copy": "first copy", "id": "toolu_01AAAAAAAAAAAAAAAAAA"},
                                {"copy": "second copy", "id": "toolu_01BBBBBBBBBBBBBBBBBB"}]},
    });
    let source = jsonl(&[
        user("u1", None, 0, json!("Hi.")),
        old,
        copies,
        assistant("a1", "x2", 3, "m1", said("Hello."), json!("end_turn")),
    ]);
    let imported = passes(&source);
    let facts = imported.fragment.facts();
    let events: Vec<Id> = find!(
        fact: Id,
        pattern!(facts, [{ ?fact @ v3::fact::modality: v3::fact::modality::EVENT }])
    )
    .collect();
    assert_eq!(events.len(), 4, "the hook, the record, and one per copy");
    let marked = annotated!(imported, v3::node::rendering);
    assert_eq!(marked.len(), 4);
    assert!(marked
        .iter()
        .all(|(_, term)| *term == v3::node::rendering::UNRECORDED));
    // The provider ids and the timing are dropped, the rest is kept.
    let hello = witnessed_saying(&imported, "Hello.");
    let window = window_before(&imported, hello);
    let held = window
        .iter()
        .filter(|(m, _)| *m == v3::fact::modality::EVENT)
        .count();
    assert_eq!(held, 4);
    // A field the table does not name still fails.
    let stray = source.replace("\"durationMs\":12", "\"durationMs\":12,\"extra\":1");
    assert_ne!(stray, source);
    assert_eq!(
        refused(&stray).path,
        "{attachment}.attachment{hook_success}.extra"
    );
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
    let first = witnessed_saying(&imported, "Searching.");
    let window = window_before(&imported, first);
    assert_eq!(
        window,
        vec![
            (v3::fact::modality::SYSTEM, Some(v3::fact::absent::BODY)),
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
    let summary = witnessed_saying(&imported, "Summary of what came before.");
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
    let three = witnessed_saying(&imported, "Three.");
    let window = window_before(&imported, three);
    assert_eq!(window.len(), 3 + 3, "system section, summary, copy, prompt");
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
    let answer = witnessed_saying(&imported, "Second answer.");
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
    let node = witnessed_saying(&imported, "Later.");
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

    // An effort the vocabulary does not name.
    let mut effort = assistant("a1", "u1", 1, "m1", said("Hello."), json!("end_turn"));
    effort["effort"] = json!("ultra");
    let source = jsonl(&[user("u1", None, 0, json!("Hi.")), effort]);
    assert_eq!(refused(&source).path, "{assistant}.effort");
    assert!(walk_rows(&source).contains(&("{assistant}.effort".to_owned(), Kind::UnknownValue)));

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

/// I1: a record whose parent is in no record of the unit opens on a span
/// of unknown items, never on a bare root. A parent written after its
/// child is still its parent.
#[test]
fn a_parent_outside_the_unit_opens_with_a_span_gap() {
    let source = jsonl(&[
        user("u2", Some("elsewhere"), 0, json!("Where were we?")),
        assistant("a2", "u2", 1, "m2", said("Here."), json!("end_turn")),
        // Written before its parent.
        user("u4", Some("a3"), 4, json!("And then?")),
        assistant("a3", "a2", 3, "m3", said("Then this."), json!("end_turn")),
    ]);
    let imported = passes(&source);
    let prompt = witnessed_saying(&imported, "Where were we?");
    let window = window_before(&imported, prompt);
    let mut expected = unrecorded_section().to_vec();
    expected.push((UNKNOWN, Some(v3::fact::absent::SPAN)));
    assert_eq!(window, expected);
    let later = witnessed_saying(&imported, "And then?");
    assert_eq!(window_before(&imported, later).len(), 4 + 3);
}

/// I2: a result renders ok unless it says it is an error, whether or not
/// the source records `is_error`; the two spellings converge.
#[test]
fn a_result_status_comes_from_the_rendering_table() {
    let run = |result: Value| {
        jsonl(&[
            user("u1", None, 0, json!("Run.")),
            assistant(
                "a1",
                "u1",
                1,
                "m1",
                call("t1", "Bash", json!({"command": "true"})),
                json!("tool_use"),
            ),
            user("u2", Some("a1"), 2, result),
        ])
    };
    let silent = run(json!([{"type": "tool_result", "tool_use_id": "t1", "content": "fine"}]));
    let spelled = run(result("t1", json!("fine")));
    let heads = |imported: &Imported| -> Vec<(Id, Id)> {
        let facts = imported.fragment.facts();
        find!(
            (fact: Id, status: Id),
            pattern!(facts, [{ ?fact @
                v3::fact::modality: v3::fact::modality::TOOL_RESULT,
                v3::fact::status: ?status,
            }])
        )
        .collect()
    };
    let silent = heads(&passes(&silent));
    assert_eq!(silent.len(), 1);
    assert_eq!(silent[0].1, v3::fact::status::OK);
    assert_eq!(silent, heads(&passes(&spelled)));
    let error = run(
        json!([{"type": "tool_result", "tool_use_id": "t1", "content": "no", "is_error": true}]),
    );
    assert_eq!(heads(&passes(&error))[0].1, v3::fact::status::ERROR);
}

/// I3: two records that hold the same node keep their claims apart: one
/// witness, one occurrence per record, each with its own time.
#[test]
fn each_record_keeps_its_claims_together() {
    let mut again = user("u3", Some("a1"), 5, json!("Once more."));
    again["sessionId"] = json!("s-2");
    let source = jsonl(&[
        user("u1", None, 0, json!("Hi.")),
        assistant("a1", "u1", 1, "m1", said("Hello."), json!("end_turn")),
        user("u2", Some("a1"), 2, json!("Once more.")),
        again,
    ]);
    let imported = passes(&source);
    let node = nodes_saying(&imported, "Once more.");
    assert_eq!(node.len(), 1, "one node, held by both records");
    let facts = imported.fragment.facts();
    let held = node[0];
    let witnesses: Vec<Id> = find!(
        witness: Id,
        pattern!(facts, [{ ?witness @ v3::witness::projects_to: held }])
    )
    .collect();
    assert_eq!(witnesses.len(), 1);
    let witnessed = witnesses[0];
    let claims: Vec<(Inline<NsTAIInterval>, Inline<Handle<UTF8String>>)> = find!(
        (time: Inline<NsTAIInterval>, session: Inline<Handle<UTF8String>>),
        pattern!(facts, [{ _?occurrence @
            v3::occurrence::of: witnessed,
            v3::witness::timestamp: ?time,
            v3::witness::source_locator: ?session,
        }])
    )
    .collect();
    assert_eq!(claims.len(), 2, "each time stays with its own session");
    let mut sessions: Vec<String> = claims.iter().map(|(_, s)| text_of(&imported, *s)).collect();
    sessions.sort();
    assert_eq!(sessions, vec!["s-1", "s-2"]);
    assert_ne!(claims[0].0, claims[1].0);
}

/// I4: every record kind the table drops as machinery is one the importer
/// names.
#[test]
fn the_importer_names_every_machinery_record_kind() {
    use crate::archive_v3::fields::{Class, DropReason};
    for entry in TABLE.entries {
        let Some(kind) = entry
            .path
            .strip_prefix('{')
            .and_then(|rest| rest.strip_suffix('}'))
        else {
            continue;
        };
        if !matches!(entry.class, Class::Drop(DropReason::Machinery(_))) || !entry.when.is_empty() {
            continue;
        }
        assert!(MACHINERY_KINDS.contains(&kind), "{kind}");
    }
}

/// I5: a title describes the nearest node on the chain of the record before
/// it, not the latest node built.
#[test]
fn a_title_describes_its_own_records_chain() {
    let mut aside = user("u3", Some("a1"), 4, json!("Never sent."));
    aside["isVisibleInTranscriptOnly"] = json!(true);
    let title = json!({"type": "ai-title", "sessionId": "s-1", "aiTitle": "A synthetic chat"});
    let source = jsonl(&[
        user("u1", None, 0, json!("Hi.")),
        assistant("a1", "u1", 1, "m1", said("Hello."), json!("end_turn")),
        user("u2", Some("a1"), 2, json!("More?")),
        assistant("a2", "u2", 3, "m2", said("More."), json!("end_turn")),
        aside,
        title,
    ]);
    let imported = passes(&source);
    let facts = imported.fragment.facts();
    let described: Vec<Id> = find!(
        node: Id,
        pattern!(facts, [{ _?gloss @
            v3::gloss::category: v3::gloss::category::TITLE,
            v3::gloss::describes: ?node,
        }])
    )
    .collect();
    assert_eq!(described, nodes_saying(&imported, "Hello."));
}

fn boundary(uuid: &str, logical: Option<&str>, anchor: Option<&str>) -> Value {
    let mut record = json!({
        "type": "system", "subtype": "compact_boundary", "uuid": uuid, "parentUuid": null,
        "logicalParentUuid": logical, "sessionId": "s-1", "timestamp": stamp(4),
        "content": "Conversation compacted", "level": "info", "isMeta": false,
        "compactMetadata": {"trigger": "auto", "preTokens": 100}
    });
    if let Some(anchor) = anchor {
        record["compactMetadata"]["preservedSegment"] =
            json!({"headUuid": "a2", "tailUuid": "a2", "anchorUuid": anchor});
        record["compactMetadata"]["preservedMessages"] =
            json!({"uuids": ["a2"], "allUuids": ["a2"], "anchorUuid": anchor});
    }
    record
}

/// I6: a compaction summary shown in the transcript only was still sent;
/// the new root continues from the old tail, and an old tail outside the
/// unit is a span gap before the summary.
#[test]
fn compaction_summaries_are_sent_and_continue() {
    let mut summary = user("c1", Some("b1"), 5, json!("Summary of what came before."));
    summary["isCompactSummary"] = json!(true);
    summary["isVisibleInTranscriptOnly"] = json!(true);
    let head = [
        user("u1", None, 0, json!("First.")),
        assistant("a1", "u1", 1, "m1", said("One."), json!("end_turn")),
        user("u2", Some("a1"), 2, json!("Second.")),
        assistant("a2", "u2", 3, "m2", said("Two."), json!("end_turn")),
    ];
    let mut records = head.to_vec();
    records.push(boundary("b1", Some("a2"), Some("c1")));
    records.push(summary.clone());
    records.push(user("u3", Some("c1"), 6, json!("Third.")));
    let source = jsonl(&records);
    let imported = passes(&source);
    let summary_node = witnessed_saying(&imported, "Summary of what came before.");
    let continues = annotated!(imported, v3::node::continues);
    assert_eq!(continues.len(), 1);
    assert_eq!(continues[0].0, summary_node);

    // The old context outside the unit: a span gap, and nothing continues.
    let outside = jsonl(&[
        boundary("b1", Some("gone"), None),
        summary.clone(),
        user("u3", Some("c1"), 6, json!("Third.")),
    ]);
    let imported = passes(&outside);
    let summary_node = witnessed_saying(&imported, "Summary of what came before.");
    let window = window_before(&imported, summary_node);
    assert_eq!(
        window.last(),
        Some(&(UNKNOWN, Some(v3::fact::absent::SPAN)))
    );
    assert!(annotated!(imported, v3::node::continues).is_empty());

    // A boundary that names no old context fails.
    let mut nameless = boundary("b1", None, None);
    nameless
        .as_object_mut()
        .unwrap()
        .remove("logicalParentUuid");
    let source = jsonl(&[
        nameless,
        summary,
        user("u3", Some("c1"), 6, json!("Third.")),
    ]);
    assert_eq!(refused(&source).path, "{system}.logicalParentUuid");

    // A new context holding nothing that continues fails.
    let mut records = head.to_vec();
    records.push(boundary("b1", Some("a2"), None));
    let source = jsonl(&records);
    assert_eq!(refused(&source).path, "{system}.logicalParentUuid");
}

/// I7: a request's window edits apply to that request alone. A later
/// request without edits is sampled from the unedited history again.
#[test]
fn window_edits_are_per_request() {
    let mut cleared = assistant("a3", "u3", 5, "m3", said("Later."), json!("end_turn"));
    cleared["message"]["context_management"] = json!({"applied_edits": [
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
        cleared,
        user("u4", Some("a3"), 6, json!("Again?")),
        assistant("a4", "u4", 7, "m4", said("Again."), json!("end_turn")),
    ]);
    let imported = passes(&source);
    let later = witnessed_saying(&imported, "Later.");
    assert!(window_before(&imported, later)
        .contains(&(v3::fact::modality::TEXT, Some(v3::fact::absent::BODY))));
    // The prompt after the edited request continues its window.
    let again = nodes_saying(&imported, "Again?");
    assert!(again.iter().any(|&n| window_before(&imported, n)
        .contains(&(v3::fact::modality::TEXT, Some(v3::fact::absent::BODY)))));
    // The next request is sampled from the whole history: the listing is
    // back and no gap is in its window.
    let answer = witnessed_saying(&imported, "Again.");
    let window = window_before(&imported, answer);
    assert!(!window.contains(&(v3::fact::modality::TEXT, Some(v3::fact::absent::BODY))));
    let listing = nodes_saying(&imported, "a long listing");
    assert_eq!(
        listing.len(),
        1,
        "the unedited history is the original line"
    );
    let mut cursor = previous(&imported, answer);
    let mut found = false;
    while let Some(at) = cursor {
        found |= at == listing[0];
        cursor = previous(&imported, at);
    }
    assert!(found, "the listing is in the window she was sampled from");
}

/// S1: each request opens with the settings header it was sampled under. A
/// changed effort branches a new window with the new header.
#[test]
fn effort_is_the_settings_header_of_each_request() {
    let effort = |record: &mut Value, value: &str| {
        record["effort"] = json!(value);
        record["perTurnEffort"] = json!(value);
    };
    let mut a1 = assistant("a1", "u1", 1, "m1", said("One."), json!("end_turn"));
    effort(&mut a1, "high");
    let mut a2 = assistant("a2", "u2", 3, "m2", said("Two."), json!("end_turn"));
    effort(&mut a2, "high");
    a2["perTurnEffort"] = json!(null);
    let mut a3 = assistant("a3", "u3", 5, "m3", said("Three."), json!("end_turn"));
    effort(&mut a3, "xhigh");
    let source = jsonl(&[
        user("u1", None, 0, json!("First.")),
        a1,
        user("u2", Some("a1"), 2, json!("Second.")),
        a2,
        user("u3", Some("a2"), 4, json!("Third.")),
        a3,
    ]);
    let imported = passes(&source);
    let facts = imported.fragment.facts();
    let headers: Vec<(Id, Id)> = find!(
        (fact: Id, effort: Id),
        pattern!(facts, [{ ?fact @ v3::fact::effort: ?effort }])
    )
    .collect();
    assert_eq!(headers.len(), 2, "high and xhigh");
    let header_of = |node: Id| -> Option<Id> {
        let mut cursor = Some(node);
        let mut first = None;
        while let Some(at) = cursor {
            first = Some(at);
            cursor = previous(&imported, at);
        }
        let root = first?;
        find!(
            effort: Id,
            pattern!(facts, [
                { root @ v3::node::holds: _?fact },
                { _?fact @ v3::fact::effort: ?effort },
            ])
        )
        .next()
    };
    let headers_of = |text: &str| -> BTreeSet<Option<Id>> {
        nodes_saying(&imported, text)
            .into_iter()
            .map(header_of)
            .collect()
    };
    use v3::fact::effort::{HIGH, XHIGH};
    // Her first two answers were sampled under high and are re-presented
    // under xhigh; the third was sampled under xhigh.
    assert_eq!(
        headers_of("One."),
        BTreeSet::from([Some(HIGH), Some(XHIGH)])
    );
    assert_eq!(
        headers_of("Two."),
        BTreeSet::from([Some(HIGH), Some(XHIGH)])
    );
    assert_eq!(headers_of("Three."), BTreeSet::from([Some(XHIGH)]));
    // The first prompt is re-presented under each header.
    assert_eq!(nodes_saying(&imported, "First.").len(), 3);

    // A turn effort that is not the request's effort fails.
    let differs = source.replacen("\"perTurnEffort\":\"high\"", "\"perTurnEffort\":\"low\"", 1);
    assert_ne!(differs, source);
    assert_eq!(refused(&differs).path, "{assistant}.perTurnEffort");
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
