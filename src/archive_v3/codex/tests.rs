//! Tests of the v3 Codex importer, on synthetic rollouts only: one per
//! format generation (G0..G4), then the window rules one at a time. Every
//! fixture is gated: the importer's fragment must pass the gate against the
//! same source bytes.

use std::collections::BTreeMap;

use anybytes::View;
use serde_json::{json, Value};
use triblespace::core::blob::MemoryBlobStoreSnapshot;
use triblespace::core::repo::BlobStoreGet;
use triblespace::prelude::blobencodings::UTF8String;
use triblespace::prelude::inlineencodings::Handle;

use super::*;
use crate::archive_v3::gate::{self, Kind, Report};

const REASONING: &str = "c2VhbGVkIHJlYXNvbmluZyA_Pn4g-_8=";
const COMPACTION: &str = "c2VhbGVkIGNvbXBhY3Rpb24g--8=";
const SEALED_MESSAGE: &str = "c2VhbGVkIG1lc3NhZ2Ug_g==";
const PNG: &str = "data:image/png;base64,iVBORw0KGgo=";

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

fn passes(source: &str) -> Imported {
    let imported = import_ok(source);
    let report = gate(source, &imported);
    assert!(report.passed(), "{report}");
    imported
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

fn stamp(second: u32) -> String {
    format!("2026-03-01T10:00:{second:02}.000Z")
}

/// A G2+ envelope.
fn line(second: u32, kind: &str, payload: Value) -> Value {
    json!({"timestamp": stamp(second), "type": kind, "payload": payload})
}

fn text_of(imported: &Imported, handle: Inline<Handle<UTF8String>>) -> String {
    imported
        .blobs
        .get::<View<str>, UTF8String>(handle)
        .unwrap()
        .as_ref()
        .to_owned()
}

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

fn witnessed(imported: &Imported, node: Id) -> bool {
    let facts = imported.fragment.facts();
    exists!(
        (),
        pattern!(facts, [{ _?witness @ v3::witness::projects_to: node }])
    )
}

fn witnessed_saying(imported: &Imported, text: &str) -> Id {
    let held: Vec<Id> = nodes_saying(imported, text)
        .into_iter()
        .filter(|&node| witnessed(imported, node))
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

/// The payloads along the window before `node`, root first (`None` for a
/// node whose fact holds no text).
fn texts_before(imported: &Imported, node: Id) -> Vec<Option<String>> {
    let facts = imported.fragment.facts();
    let mut out = Vec::new();
    let mut cursor = previous(imported, node);
    while let Some(at) = cursor {
        let found: Vec<Inline<Handle<UTF8String>>> = find!(
            payload: Inline<Handle<UTF8String>>,
            pattern!(facts, [
                { at @ v3::node::holds: _?fact },
                { _?fact @ v3::fact::payload: ?payload },
            ])
        )
        .collect();
        out.push(found.first().map(|handle| text_of(imported, *handle)));
        cursor = previous(imported, at);
    }
    out.reverse();
    out
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

fn glosses(imported: &Imported) -> Vec<String> {
    let facts = imported.fragment.facts();
    let found: Vec<Inline<Handle<UTF8String>>> = find!(
        text: Inline<Handle<UTF8String>>,
        pattern!(facts, [{ _?gloss @
            v3::gloss::category: v3::gloss::category::REASONING_SUMMARY,
            v3::gloss::text: ?text,
        }])
    )
    .collect();
    found.into_iter().map(|t| text_of(imported, t)).collect()
}

// ---------------------------------------------------------------------------
// One fixture per format generation.

/// G0 (2025-07): a header, then bare items; a local shell call whose
/// output is the JSON text she was shown.
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

#[test]
fn g0_passes() {
    let imported = passes(&g0());
    let answer = witnessed_saying(&imported, "Two files: a and b.");
    let before = texts_before(&imported, answer);
    // header gap, system gap, tools gap, prompt, call, result head, output.
    assert_eq!(before.len(), 7);
    assert_eq!(before[3].as_deref(), Some("List the files."));
    assert!(before[6].as_deref().unwrap().contains("exit_code"));
    // The output answers the call.
    assert_eq!(annotated!(imported, v3::node::responds_to).len(), 1);
}

/// G1 (2025-08..10): the header adds instructions (sent as a user message)
/// and git; state lines; reasoning with and without its sealed token.
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

#[test]
fn g1_passes() {
    let imported = passes(&g1());
    let mut summaries = glosses(&imported);
    summaries.sort();
    assert_eq!(
        summaries,
        ["**Planning** the run.", "**Reading** the output."]
    );
    // Both glosses describe a thinking node: the sealed token, and a body
    // gap where the token was not recorded.
    let described = annotated!(imported, v3::gloss::describes);
    assert_eq!(described.len(), 2);
    // The header instructions are a copy of the user message.
    let changed = g1().replace(
        "\"instructions\":\"Be brief.\"",
        "\"instructions\":\"Be long.\"",
    );
    assert_ne!(changed, g1());
    assert_eq!(refused(&changed).path, ".instructions");
    assert!(walk_rows(&changed).contains(&(".instructions".to_owned(), Kind::Mirror)));
}

/// G2 (2025-11 on): envelopes, the settings header, client copies of
/// items, an aborted turn, an erred turn, and a summary-only compaction.
fn g2() -> String {
    jsonl(&[
        line(
            0,
            "session_meta",
            json!({"id": "33333333-3333-4333-8333-333333333333",
            "timestamp": stamp(0), "cwd": "/synthetic", "originator": "codex_cli_rs",
            "cli_version": "0.0.0", "instructions": null, "source": "cli",
            "model_provider": "openai"}),
        ),
        line(
            1,
            "response_item",
            json!({"type": "message", "role": "developer",
            "content": [{"type": "input_text", "text": "Sandbox: workspace-write."}]}),
        ),
        line(
            2,
            "event_msg",
            json!({"type": "task_started", "turn_id": "t1",
            "model_context_window": 1000, "collaboration_mode_kind": "default"}),
        ),
        line(
            3,
            "turn_context",
            json!({"cwd": "/synthetic", "approval_policy": "on-request",
            "sandbox_policy": {"type": "workspace-write", "network_access": false},
            "model": "gpt-5-codex", "effort": "high", "summary": "auto",
            "user_instructions": "Sandbox: workspace-write."}),
        ),
        line(
            4,
            "response_item",
            json!({"type": "message", "role": "user",
            "content": [{"type": "input_text", "text": "Fix the bug."}]}),
        ),
        line(
            4,
            "event_msg",
            json!({"type": "user_message", "message": "Fix the bug.",
            "images": [], "local_images": [], "text_elements": []}),
        ),
        line(
            5,
            "response_item",
            json!({"type": "reasoning", "summary": [
            {"type": "summary_text", "text": "**Looking** at it."}],
            "content": null, "encrypted_content": REASONING}),
        ),
        line(
            5,
            "event_msg",
            json!({"type": "agent_reasoning", "text": "**Looking** at it."}),
        ),
        line(
            6,
            "response_item",
            json!({"type": "message", "role": "assistant", "phase": "commentary",
            "content": [{"type": "output_text", "text": "Checking the parser."}]}),
        ),
        line(
            6,
            "event_msg",
            json!({"type": "agent_message", "message": "Checking the parser.",
            "phase": "commentary"}),
        ),
        line(
            7,
            "event_msg",
            json!({"type": "token_count", "info": null, "rate_limits": null}),
        ),
        line(
            8,
            "event_msg",
            json!({"type": "turn_aborted", "reason": "interrupted", "turn_id": "t1"}),
        ),
        line(
            9,
            "event_msg",
            json!({"type": "task_started", "turn_id": "t2"}),
        ),
        line(
            9,
            "turn_context",
            json!({"cwd": "/synthetic", "model": "gpt-5-codex",
            "effort": "xhigh", "summary": "auto"}),
        ),
        line(
            10,
            "response_item",
            json!({"type": "message", "role": "user",
            "content": [{"type": "input_text", "text": "Go on."}]}),
        ),
        line(
            11,
            "event_msg",
            json!({"type": "task_complete", "turn_id": "t2",
            "last_agent_message": null,
            "error": {"message": "stream disconnected", "codex_error_info": "other"}}),
        ),
        line(
            12,
            "compacted",
            json!({"message": "The bug is in the parser."}),
        ),
        line(
            13,
            "response_item",
            json!({"type": "message", "role": "assistant", "phase": "final_answer",
            "content": [{"type": "output_text", "text": "Fixed."}]}),
        ),
    ])
}

#[test]
fn g2_passes() {
    let imported = passes(&g2());
    // The aborted turn ends at her last node; the erred one at the prompt.
    let ends = annotated!(imported, v3::node::ends);
    let commentary = witnessed_saying(&imported, "Checking the parser.");
    assert!(ends.contains(&(commentary, v3::node::ends::INTERRUPTED)));
    assert_eq!(ends.len(), 2);
    // The effort change re-presents the line: the prompt has a copy that
    // recalls it.
    assert_eq!(nodes_saying(&imported, "Fix the bug.").len(), 2);
    assert_eq!(annotated!(imported, v3::node::recalls).len() >= 1, true);
    // After the compaction: header, system, tools, span gap, summary.
    let fixed = witnessed_saying(&imported, "Fixed.");
    let before = texts_before(&imported, fixed);
    assert_eq!(before.len(), 5);
    assert_eq!(before[4].as_deref(), Some("The bug is in the parser."));
    assert_eq!(annotated!(imported, v3::node::continues).len(), 1);
}

#[test]
fn a_copy_without_its_original_fails() {
    let source = g2().replace(
        "\"message\":\"Checking the parser.\",\"phase\"",
        "\"message\":\"A subagent spoke.\",\"phase\"",
    );
    assert_ne!(source, g2());
    assert_eq!(
        refused(&source).path,
        "{event_msg}.payload{agent_message}.message"
    );
    assert!(walk_rows(&source).contains(&(
        "{event_msg}.payload{agent_message}.message".to_owned(),
        Kind::Mirror
    )));
}

/// G3 (2026-07 on): base instructions, a subagent's header, world state
/// mirrored in a developer message, another agent's message, namespaced
/// and custom calls, image output, a replacement-history compaction.
fn g3() -> String {
    jsonl(&[
        line(
            0,
            "session_meta",
            json!({"id": "44444444-4444-4444-8444-444444444444",
            "timestamp": stamp(0), "cwd": "/synthetic", "originator": "codex_exec",
            "cli_version": "0.0.0", "model_provider": "openai",
            "base_instructions": {"text": "You are a coding agent."},
            "agent_nickname": "Ada", "agent_path": "/root/ada",
            "parent_thread_id": "55555555-5555-4555-8555-555555555555",
            "source": {"subagent": {"thread_spawn": {
                "parent_thread_id": "55555555-5555-4555-8555-555555555555",
                "depth": 1, "agent_nickname": "Ada", "agent_path": "/root/ada", "agent_role": null}}},
            "history_mode": "legacy", "multi_agent_version": "2",
            "context_window": {"window_id": "w1"}}),
        ),
        line(
            1,
            "world_state",
            json!({"full": true, "state": {
            "agents_md": {"directory": "/synthetic", "text": "Use tabs."},
            "apps_instructions": false, "skills": {"includeInstructions": true}}}),
        ),
        line(
            1,
            "response_item",
            json!({"type": "message", "role": "developer",
            "content": [{"type": "input_text", "text": "Instructions for /synthetic:\nUse tabs."}]}),
        ),
        line(
            2,
            "turn_context",
            json!({"cwd": "/synthetic", "model": "gpt-5.5", "effort": "ultra",
            "summary": "detailed", "collaboration_mode": {"mode": "default",
                "settings": {"model": "gpt-5.5", "reasoning_effort": "ultra",
                    "developer_instructions": null}}}),
        ),
        line(
            3,
            "inter_agent_communication_metadata",
            json!({"trigger_turn": true}),
        ),
        line(
            3,
            "response_item",
            json!({"type": "agent_message", "author": "/root",
            "recipient": "/root/ada",
            "internal_chat_message_metadata_passthrough": {"turn_id": "t1"},
            "content": [{"type": "input_text", "text": "Count the lines."},
                        {"type": "encrypted_content", "encrypted_content": SEALED_MESSAGE}]}),
        ),
        line(
            4,
            "response_item",
            json!({"type": "function_call", "name": "wc",
            "namespace": "mcp__tools", "arguments": "{\"path\":\"a.txt\"}", "call_id": "c1",
            "internal_chat_message_metadata_passthrough": {"turn_id": "t1"}}),
        ),
        line(
            5,
            "response_item",
            json!({"type": "function_call_output", "call_id": "c1",
            "output": [{"type": "input_text", "text": "3 a.txt"},
                       {"type": "input_image", "image_url": PNG, "detail": "original"}]}),
        ),
        line(
            6,
            "response_item",
            json!({"type": "custom_tool_call", "status": "completed",
            "call_id": "c2", "name": "apply_patch", "input": "*** Begin Patch\n*** End Patch\n"}),
        ),
        line(
            7,
            "response_item",
            json!({"type": "custom_tool_call_output", "call_id": "c2",
            "output": "Done!"}),
        ),
        line(
            8,
            "response_item",
            json!({"type": "web_search_call", "status": "completed",
            "action": {"type": "search", "query": "line counts"}}),
        ),
        line(
            9,
            "compacted",
            json!({"message": "",
            "replacement_history": [
                {"type": "message", "role": "developer",
                 "content": [{"type": "input_text", "text": "Instructions for /synthetic:\nUse tabs."}]},
                {"type": "agent_message", "author": "/root", "recipient": "/root/ada",
                 "content": [{"type": "input_text", "text": "Count the lines."}]},
                {"type": "compaction", "encrypted_content": COMPACTION}
            ],
            "window_id": "w2", "previous_window_id": "w1", "first_window_id": "w1",
            "window_number": 2}),
        ),
        line(
            10,
            "response_item",
            json!({"type": "message", "role": "assistant",
            "phase": "final_answer",
            "content": [{"type": "output_text", "text": "Three lines."}]}),
        ),
    ])
}

#[test]
fn g3_passes() {
    let imported = passes(&g3());
    // The developer message came before the first header: under the header
    // it is a copy that recalls it. The compaction re-presents the same
    // prefix, so its items converge on the nodes already there.
    assert_eq!(
        nodes_saying(&imported, "Instructions for /synthetic:\nUse tabs.").len(),
        2
    );
    assert_eq!(nodes_saying(&imported, "Count the lines.").len(), 1);
    assert!(!annotated!(imported, v3::node::recalls).is_empty());
    // The new line continues the old one.
    assert_eq!(annotated!(imported, v3::node::continues).len(), 1);
    let answer = witnessed_saying(&imported, "Three lines.");
    let before = texts_before(&imported, answer);
    // header, system, tools, developer, agent text, sealed compaction.
    assert_eq!(before.len(), 6);
    assert_eq!(before[1].as_deref(), Some("You are a coding agent."));
}

#[test]
fn unmirrored_world_state_fails() {
    let source = g3().replace("\"text\":\"Use tabs.\"", "\"text\":\"Use spaces.\"");
    assert_ne!(source, g3());
    assert_eq!(
        refused(&source).path,
        "{world_state}.payload.state.agents_md.text"
    );
}

/// G4 (2026-08 on): ordinals, item events, token usage records, response
/// metadata, and a rollback.
fn g4() -> String {
    let mut records = vec![
        line(
            0,
            "session_meta",
            json!({"id": "66666666-6666-4666-8666-666666666666",
            "timestamp": stamp(0), "cwd": "/synthetic", "originator": "codex_cli_rs",
            "cli_version": "0.0.0", "model_provider": "openai",
            "base_instructions": {"text": "You are a coding agent."}}),
        ),
        line(
            1,
            "event_msg",
            json!({"type": "task_started", "turn_id": "t1"}),
        ),
        line(
            1,
            "turn_context",
            json!({"model": "gpt-5.5", "effort": "medium", "summary": "auto",
            "turn_id": "t1"}),
        ),
        line(
            2,
            "response_item",
            json!({"type": "message", "role": "user",
            "content": [{"type": "input_text", "text": "First try."}]}),
        ),
        line(
            3,
            "response_item",
            json!({"type": "message", "role": "assistant",
            "phase": "final_answer", "content": [{"type": "output_text", "text": "Answer one."}]}),
        ),
        line(
            3,
            "event_msg",
            json!({"type": "item_completed", "thread_id": "x", "turn_id": "t1",
            "item": {"type": "AgentMessage", "id": "m1", "phase": "final_answer",
                "content": [{"type": "Text", "text": "Answer one."}]}}),
        ),
        line(
            4,
            "token_usage_record",
            json!({"usage": {"input_tokens": 1}}),
        ),
        line(
            5,
            "event_msg",
            json!({"type": "thread_rolled_back", "num_turns": 1}),
        ),
        line(
            6,
            "event_msg",
            json!({"type": "task_started", "turn_id": "t2"}),
        ),
        line(
            6,
            "turn_context",
            json!({"model": "gpt-5.5", "effort": "medium", "summary": "auto",
            "turn_id": "t2"}),
        ),
        line(
            7,
            "response_item",
            json!({"type": "message", "role": "user",
            "content": [{"type": "input_text", "text": "Second try."}]}),
        ),
        line(
            8,
            "response_item",
            json!({"type": "reasoning", "summary": [],
            "content": null, "encrypted_content": REASONING}),
        ),
        line(
            8,
            "event_msg",
            json!({"type": "item_completed", "thread_id": "x", "turn_id": "t2",
            "item": {"type": "Reasoning", "id": "r1", "summary_text": [], "raw_content": []}}),
        ),
    ];
    let mut tail = line(
        9,
        "response_item",
        json!({"type": "message", "role": "assistant",
        "phase": "final_answer", "content": [{"type": "output_text", "text": "Answer two."}]}),
    );
    tail["metadata"] = json!({"client_authored": false, "user_input_order": 2});
    records.push(tail);
    for (at, record) in records.iter_mut().enumerate() {
        record["ordinal"] = json!(at);
    }
    jsonl(&records)
}

#[test]
fn g4_passes_and_a_rollback_branches() {
    let imported = passes(&g4());
    let second = witnessed_saying(&imported, "Second try.");
    // The rolled-back turn is gone: the second prompt follows the system
    // section directly.
    let before = texts_before(&imported, second);
    assert_eq!(before.len(), 3);
    assert_eq!(before[1].as_deref(), Some("You are a coding agent."));
    let first = witnessed_saying(&imported, "First try.");
    assert_eq!(previous(&imported, first), previous(&imported, second));
}

#[test]
fn a_rollback_past_the_turns_fails() {
    let source = g4().replace("\"num_turns\":1", "\"num_turns\":3");
    assert_ne!(source, g4());
    assert_eq!(
        refused(&source).path,
        "{event_msg}.payload{thread_rolled_back}.num_turns"
    );
}

#[test]
fn unknown_fields_and_kinds_fail() {
    let source = g4().replace("\"effort\":\"medium\"", "\"effort\":\"medium\",\"novel\":1");
    assert_eq!(refused(&source).path, "{turn_context}.payload.novel");
    let source = g4().replace(
        "\"type\":\"token_usage_record\"",
        "\"type\":\"novel_record\"",
    );
    assert!(walk_rows(&source)
        .iter()
        .any(|(path, kind)| path.starts_with("{novel_record}") && *kind == Kind::Unknown));
    let source = g4().replace("\"effort\":\"medium\"", "\"effort\":\"enormous\"");
    assert_eq!(refused(&source).path, "{turn_context}.payload.effort");
    // An open field fails.
    let source = g3().replace(
        "\"apps_instructions\":false",
        "\"apps_instructions\":false,\"multi_agent_usage_hint\":\"hint\"",
    );
    assert_eq!(
        refused(&source).path,
        "{world_state}.payload.state.multi_agent_usage_hint"
    );
}

#[test]
fn the_gate_catches_a_lost_item() {
    // Import a unit, then gate it against a source with one more item: the
    // gate sees what the importer never had.
    let imported = import_ok(&g0());
    let longer = g0().replace(
        "{\"text\":\"Two files: a and b.\",\"type\":\"output_text\"}",
        "{\"text\":\"Two files: a and b.\",\"type\":\"output_text\"},\
         {\"text\":\"Done.\",\"type\":\"output_text\"}",
    );
    assert_ne!(longer, g0());
    let report = gate(&longer, &imported);
    assert!(!report.passed());
    assert!(report.rows.iter().any(|row| row.kind == Kind::Missing));
}

// ---------------------------------------------------------------------------
// The real sample. Prints only paths, kinds and counts.

#[test]
#[ignore = "reads real rollouts named by AV3_SAMPLE; prints no values"]
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
    let mut unread = 0;
    let mut first_failures: BTreeMap<(String, &'static str), usize> = BTreeMap::new();
    let mut walk_failures: BTreeMap<(String, String), usize> = BTreeMap::new();
    let mut gate_failures: BTreeMap<(String, String), usize> = BTreeMap::new();
    let mut by_month: BTreeMap<String, [usize; 3]> = BTreeMap::new();
    for file in &files {
        let Ok(bytes) = std::fs::read(file) else {
            unread += 1;
            continue;
        };
        let month = file
            .rsplit('/')
            .next()
            .and_then(|name| name.strip_prefix("rollout-"))
            .map(|rest| rest.chars().take(7).collect::<String>())
            .unwrap_or_default();
        let walked = gate::walk::walk(&TABLE, &bytes);
        let mut seen = std::collections::BTreeSet::new();
        for (path, kind, _) in &walked.rows {
            seen.insert((gate::normalize(path), kind.to_string()));
        }
        for row in seen {
            *walk_failures.entry(row).or_default() += 1;
        }
        drop(walked);
        let counts = by_month.entry(month).or_default();
        match import(&bytes) {
            Err(failure) => {
                import_failed += 1;
                counts[1] += 1;
                *first_failures
                    .entry((gate::normalize(&failure.path), failure.reason))
                    .or_default() += 1;
            }
            Ok(mut fragment) => {
                let blobs = fragment.blobs_mut().snapshot().unwrap();
                let report = gate::check(&TABLE, &bytes, fragment.facts(), &blobs);
                if report.passed() {
                    passed += 1;
                    counts[0] += 1;
                } else {
                    gate_failed += 1;
                    counts[2] += 1;
                    let mut seen = std::collections::BTreeSet::new();
                    for row in report.rows {
                        seen.insert((row.path, row.kind.to_string()));
                    }
                    for row in seen {
                        *gate_failures.entry(row).or_default() += 1;
                    }
                }
            }
        }
    }
    println!(
        "files {} unread {} passed {} import-failed {} gate-failed {}",
        files.len(),
        unread,
        passed,
        import_failed,
        gate_failed
    );
    println!("-- by month: pass, import-fail, gate-fail");
    for (month, [p, i, g]) in &by_month {
        println!("{month}\t{p}\t{i}\t{g}");
    }
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
