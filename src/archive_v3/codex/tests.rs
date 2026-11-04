//! Tests of the v3 Codex importer, on synthetic rollouts only: one per
//! format generation (G0..G4), then the window rules one at a time. Every
//! fixture is gated: the importer's fragment must pass the gate against the
//! same source bytes.

use std::collections::{BTreeMap, BTreeSet};

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

/// Why her samplings stopped (M1): `(node, term)` for every `ends` of an
/// occurrence, by the node its witness projects to. No node carries one.
fn ends_on(imported: &Imported) -> Vec<(Id, Id)> {
    let facts = imported.fragment.facts();
    let on_nodes = find!(
        node: Id,
        pattern!(facts, [{ ?node @ v3::node::holds: _?fact, v3::node::ends: _?term }])
    )
    .count();
    assert_eq!(on_nodes, 0, "a node carries ends (M1)");
    let mut found: Vec<(Id, Id)> = find!(
        (node: Id, term: Id),
        pattern!(facts, [
            { _?witness @ v3::witness::projects_to: ?node },
            { _?occurrence @ v3::occurrence::of: _?witness, v3::witness::ends: ?term },
        ])
    )
    .collect();
    found.sort();
    found
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
            "event_msg",
            json!({"type": "agent_message", "message": "Checking the parser.",
            "phase": "commentary"}),
        ),
        line(
            6,
            "response_item",
            json!({"type": "message", "role": "assistant", "phase": "commentary",
            "content": [{"type": "output_text", "text": "Checking the parser."}]}),
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
    // The aborted turn ends at her last node. The erred turn holds only his
    // prompt: no ends there, and no fact for the error (X5, X6).
    let ends = ends_on(&imported);
    let commentary = witnessed_saying(&imported, "Checking the parser.");
    assert!(ends.contains(&(commentary, v3::node::ends::INTERRUPTED)));
    assert_eq!(ends.len(), 1);
    let facts = imported.fragment.facts();
    assert!(!exists!(
        (),
        pattern!(facts, [{ _?fact @ v3::fact::modality: v3::fact::modality::EVENT }])
    ));
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
        .any(|(path, kind)| path.starts_with("{<value>}") && *kind == Kind::Unknown));
    let source = g4().replace("\"effort\":\"medium\"", "\"effort\":\"enormous\"");
    assert_eq!(refused(&source).path, "{turn_context}.payload.effort");
    // An open field fails.
    let source = g4().replace(
        "\"client_authored\":false",
        "\"client_authored\":false,\"retained_source\":\"x\"",
    );
    assert_ne!(source, g4());
    assert_eq!(
        refused(&source).path,
        "{response_item}.metadata.retained_source"
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
// Round 3: the rulings X1..X10, and the losslessness review's fixtures
// (rv_*), each of which failed against the round-2 importer.

fn rv_meta(second: u32, id: &str, base: &str) -> Value {
    line(
        second,
        "session_meta",
        json!({"id": id, "timestamp": stamp(second), "cwd": "/synthetic",
            "originator": "codex_cli_rs", "cli_version": "0.0.0", "model_provider": "openai",
            "base_instructions": {"text": base}}),
    )
}

fn rv_turn(second: u32, turn: &str, effort: &str) -> [Value; 2] {
    [
        line(
            second,
            "event_msg",
            json!({"type": "task_started", "turn_id": turn}),
        ),
        line(
            second,
            "turn_context",
            json!({"model": "gpt-5.5", "effort": effort, "summary": "auto", "turn_id": turn}),
        ),
    ]
}

fn rv_user(second: u32, text: &str) -> Value {
    line(
        second,
        "response_item",
        json!({"type": "message", "role": "user",
            "content": [{"type": "input_text", "text": text}]}),
    )
}

fn rv_said(second: u32, text: &str, phase: Option<&str>) -> Value {
    let mut item = json!({"type": "message", "role": "assistant",
        "content": [{"type": "output_text", "text": text}]});
    if let Some(phase) = phase {
        item["phase"] = json!(phase);
    }
    line(second, "response_item", item)
}

fn rv_head(records: &mut Vec<Value>) {
    records.push(rv_meta(0, "77777777-7777-4777-8777-777777777777", "Sys."));
    records.extend(rv_turn(1, "t1", "high"));
}

fn chain(imported: &Imported, node: Id) -> Vec<Id> {
    let mut out = vec![node];
    let mut cursor = previous(imported, node);
    while let Some(at) = cursor {
        out.push(at);
        cursor = previous(imported, at);
    }
    out
}

fn some(texts: &[Option<String>]) -> Vec<String> {
    texts.iter().flatten().cloned().collect()
}

/// Whether a path along `previous` and `continues` returns to where a
/// `continues` link starts.
fn continues_cycle(imported: &Imported) -> bool {
    let facts = imported.fragment.facts();
    let mut next: BTreeMap<Id, Vec<Id>> = BTreeMap::new();
    for (node, previous) in find!(
        (node: Id, previous: Id),
        pattern!(facts, [{ ?node @ v3::node::previous: ?previous }])
    ) {
        next.entry(node).or_default().push(previous);
    }
    let links = annotated!(imported, v3::node::continues);
    for &(node, target) in &links {
        next.entry(node).or_default().push(target);
    }
    links.iter().any(|&(start, target)| {
        let mut seen = BTreeSet::new();
        let mut stack = vec![target];
        while let Some(at) = stack.pop() {
            if at == start {
                return true;
            }
            if seen.insert(at) {
                stack.extend(next.get(&at).into_iter().flatten().copied());
            }
        }
        false
    })
}

fn registers(imported: &Imported, node: Id) -> Vec<Id> {
    let facts = imported.fragment.facts();
    find!(
        r: Id,
        pattern!(facts, [
            { node @ v3::node::holds: _?fact },
            { _?fact @ v3::fact::register: ?r },
        ])
    )
    .collect()
}

/// X1 (review CX-2b, refuted in round 2): the phase only the event copy
/// carries is her message's register; the gate supplies it too.
#[test]
fn rv_cx2b_phase_only_on_event() {
    let mut records = Vec::new();
    rv_head(&mut records);
    records.push(rv_user(2, "Q."));
    records.push(line(
        3,
        "event_msg",
        json!({"type": "agent_message", "message": "Looking.", "phase": "commentary"}),
    ));
    records.push(rv_said(3, "Looking.", None));
    let source = jsonl(&records);
    let imported = passes(&source);
    let looking = witnessed_saying(&imported, "Looking.");
    assert_eq!(
        registers(&imported, looking),
        vec![v3::fact::register::COMMENTARY]
    );
    // The gate holds the importer to it: the same fragment against a source
    // whose copy names the other phase fails.
    let other = source.replace("\"phase\":\"commentary\"", "\"phase\":\"final_answer\"");
    assert_ne!(other, source);
    assert!(!gate(&other, &imported).passed());
}

/// X1 (review CX-2): where both hold the phase, a copy that disagrees, or
/// one that names a phase the table does not, fails.
#[test]
fn rv_cx2_registers() {
    let mut records = Vec::new();
    rv_head(&mut records);
    records.push(rv_user(2, "Q."));
    records.push(line(
        3,
        "event_msg",
        json!({"type": "agent_message", "message": "Looking.", "phase": "commentary"}),
    ));
    records.push(rv_said(3, "Looking.", Some("commentary")));
    records.push(line(
        4,
        "event_msg",
        json!({"type": "agent_message", "message": "Answer.", "phase": "final_answer"}),
    ));
    records.push(rv_said(4, "Answer.", Some("final_answer")));
    records.push(rv_said(5, "Bare.", None));
    let source = jsonl(&records);
    let imported = passes(&source);
    let of = |text: &str| registers(&imported, witnessed_saying(&imported, text));
    assert_eq!(of("Looking."), vec![v3::fact::register::COMMENTARY]);
    assert_eq!(of("Answer."), vec![v3::fact::register::ANSWER]);
    assert!(of("Bare.").is_empty());
    let swapped = source.replacen(
        "\"message\":\"Looking.\",\"phase\":\"commentary\"",
        "\"message\":\"Looking.\",\"phase\":\"final_answer\"",
        1,
    );
    assert_ne!(swapped, source);
    assert_eq!(
        refused(&swapped).path,
        "{event_msg}.payload{agent_message}.phase"
    );
    assert!(walk_rows(&swapped).contains(&(
        "{event_msg}.payload{agent_message}.phase".to_owned(),
        Kind::Mirror
    )));
    // Her message (the copy before it keeps its phase) under a phase no
    // entry names.
    let mut novel = records.clone();
    let at = novel
        .iter()
        .position(|record| record["payload"]["content"][0]["text"] == json!("Looking."))
        .unwrap();
    novel[at]["payload"]["phase"] = json!("aside");
    let novel = jsonl(&novel);
    // Her text under a phase no entry names is unknown; so is the phase.
    refused(&novel);
    assert!(walk_rows(&novel).contains(&(
        "{response_item}.payload{message}{assistant}.phase".to_owned(),
        Kind::UnknownValue
    )));
}

fn rv_compaction_source(history: Value, after: &[Value]) -> String {
    let mut records = Vec::new();
    rv_head(&mut records);
    records.push(rv_user(2, "A."));
    records.push(rv_said(3, "B.", None));
    records.extend(rv_turn(4, "t2", "high"));
    records.push(rv_user(5, "C."));
    records.push(rv_said(6, "D.", None));
    records.push(line(
        7,
        "compacted",
        json!({"message": "", "replacement_history": history}),
    ));
    records.extend(after.iter().cloned());
    jsonl(&records)
}

/// X2 (review CX-3, keep-A refuted in round 2): `continues` sits on the
/// first node of the new line that is not on the old line, so no continues
/// path returns to its start, and a kept item is the compaction's own (no
/// correlator names an original, so it recalls nothing).
#[test]
fn rv_cx3_replacement_history() {
    let keep_c = json!([
        {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "C."}]},
        {"type": "compaction", "encrypted_content": COMPACTION}
    ]);
    let source = rv_compaction_source(keep_c, &[rv_said(8, "E.", None)]);
    let imported = passes(&source);
    let e = witnessed_saying(&imported, "E.");
    let before = texts_before(&imported, e);
    assert_eq!(some(&before), ["Sys.", "C."]);
    assert_eq!(before.len(), 5);
    let continues = annotated!(imported, v3::node::continues);
    let d = witnessed_saying(&imported, "D.");
    let c = nodes_saying(&imported, "C.");
    assert_eq!(c.len(), 2);
    assert_eq!(continues.len(), 1);
    assert_eq!(continues[0].1, d);
    assert!(c.contains(&continues[0].0));
    assert!(!continues_cycle(&imported));
    // Only the system prompt, re-presented under the first header, recalls.
    let system = witnessed_saying(&imported, "Sys.");
    assert!(annotated!(imported, v3::node::recalls)
        .iter()
        .all(|(_, original)| *original == system));

    // The kept history starts with the line's own first prompt: the new
    // line's first node is the old one, so `continues` sits on the next.
    let keep_a = json!([
        {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "A."}]},
        {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "C."}]},
        {"type": "compaction", "encrypted_content": COMPACTION}
    ]);
    let source = rv_compaction_source(keep_a, &[rv_said(8, "E.", None)]);
    let imported = passes(&source);
    let continues = annotated!(imported, v3::node::continues);
    let d = witnessed_saying(&imported, "D.");
    assert_eq!(nodes_saying(&imported, "A.").len(), 1);
    assert_eq!(continues.len(), 1);
    assert_eq!(continues[0].1, d);
    assert!(!chain(&imported, d).contains(&continues[0].0));
    assert!(!continues_cycle(&imported));

    // A new context that lies wholly on the old line has no node to carry
    // `continues`, and fails.
    let on_old = json!([
        {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "A."}]},
    ]);
    let source = rv_compaction_source(on_old, &[]);
    assert_eq!(refused(&source).path, "{compacted}.payload");

    // A kept message of hers with its phase is still open.
    let phased = json!([
        {"type": "message", "role": "assistant", "phase": "final_answer",
         "content": [{"type": "output_text", "text": "D."}]},
        {"type": "compaction", "encrypted_content": COMPACTION}
    ]);
    let source = rv_compaction_source(phased, &[rv_said(8, "E.", None)]);
    assert_eq!(
        refused(&source).path,
        "{compacted}.payload.replacement_history[]{message}{assistant}.phase"
    );

    // A rollback reaching back over the compaction fails.
    let keep_c = json!([
        {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "C."}]},
        {"type": "compaction", "encrypted_content": COMPACTION}
    ]);
    let source = rv_compaction_source(
        keep_c,
        &[
            rv_said(8, "E.", None),
            line(
                9,
                "event_msg",
                json!({"type": "thread_rolled_back", "num_turns": 1}),
            ),
            rv_user(10, "F."),
        ],
    );
    assert_eq!(
        refused(&source).path,
        "{event_msg}.payload{thread_rolled_back}.num_turns"
    );
}

/// X3 (review CX-4): a fork replays its ancestors' headers after its own.
/// Where they agree, the replay converges with the ancestor's own rollout;
/// where a replayed one differs, which records are the fork's own is
/// unmarked and the unit fails.
#[test]
fn rv_cx4_ancestor_headers() {
    let fork = |own: &str, ancestors: &[(&str, &str)]| {
        let mut meta = rv_meta(0, "88888888-8888-4888-8888-888888888888", own);
        meta["payload"]["forked_from_id"] = json!(ancestors[0].0);
        let mut records = vec![meta];
        for (id, base) in ancestors {
            records.push(rv_meta(0, id, base));
        }
        records.extend(rv_turn(1, "t1", "high"));
        records.push(rv_user(2, "Hi."));
        records.push(rv_said(3, "Hello.", None));
        records.extend(rv_turn(4, "t2", "high"));
        records.push(rv_user(5, "Next."));
        records.push(rv_said(6, "Answer.", None));
        jsonl(&records)
    };
    let s1 = "99999999-9999-4999-8999-999999999999";
    let s0 = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    let imported = passes(&fork("Sys.", &[(s1, "Sys."), (s0, "Sys.")]));
    let answer = witnessed_saying(&imported, "Answer.");
    assert_eq!(
        some(&texts_before(&imported, answer)),
        ["Sys.", "Hi.", "Hello.", "Next."]
    );
    let mut own = vec![rv_meta(0, s1, "Sys.")];
    own.extend(rv_turn(1, "t1", "high"));
    own.push(rv_user(2, "Hi."));
    own.push(rv_said(3, "Hello.", None));
    let ancestor = passes(&jsonl(&own));
    assert_eq!(
        witnessed_saying(&imported, "Hello."),
        witnessed_saying(&ancestor, "Hello.")
    );
    let differing = fork("New sys.", &[(s1, "Old sys.")]);
    assert_eq!(
        refused(&differing).path,
        "{session_meta}.payload.base_instructions.text"
    );
}

/// X4 (review CX-5): an output answering a call of a rolled-back turn
/// fails; the rollback itself branches.
#[test]
fn rv_cx5_rollback() {
    let mut records = vec![rv_meta(0, "77777777-7777-4777-8777-777777777777", "Sys.")];
    records.extend(rv_turn(1, "t1", "medium"));
    records.push(rv_user(2, "P1."));
    records.push(rv_said(3, "R1.", None));
    records.extend(rv_turn(4, "t2", "medium"));
    records.push(rv_user(5, "P2."));
    records.push(line(
        6,
        "response_item",
        json!({"type": "function_call", "name": "ls", "arguments": "{}", "call_id": "c9"}),
    ));
    records.extend(rv_turn(7, "t3", "high"));
    records.push(rv_user(8, "P3."));
    records.push(rv_said(9, "R3.", None));
    records.push(line(
        10,
        "event_msg",
        json!({"type": "thread_rolled_back", "num_turns": 2}),
    ));
    records.extend(rv_turn(11, "t4", "high"));
    records.push(rv_user(12, "P4."));
    records.push(rv_said(13, "R4.", None));
    let imported = passes(&jsonl(&records));
    let r4 = witnessed_saying(&imported, "R4.");
    assert_eq!(
        some(&texts_before(&imported, r4)),
        ["Sys.", "P1.", "R1.", "P4."]
    );
    let mut late = records.clone();
    late.push(line(
        14,
        "response_item",
        json!({"type": "function_call_output", "call_id": "c9", "output": "x"}),
    ));
    let late = jsonl(&late);
    assert_eq!(
        refused(&late).path,
        "{response_item}.payload{function_call_output}.call_id"
    );
}

/// X5 (review CX-7): `ends` only on her nodes; a turn aborted before she
/// produced anything carries none; every abort is a claim of its record,
/// on the next node placed (or the last one, at the end of the file).
#[test]
fn rv_cx7_aborted() {
    let abort = |second: u32| {
        line(
            second,
            "event_msg",
            json!({"type": "turn_aborted", "reason": "interrupted", "turn_id": "t"}),
        )
    };
    let mut records = Vec::new();
    rv_head(&mut records);
    records.push(rv_user(2, "P1."));
    records.push(rv_said(3, "Thinking aloud.", Some("commentary")));
    records.push(abort(4));
    records.extend(rv_turn(5, "t2", "high"));
    records.push(rv_user(6, "P2."));
    records.push(abort(7));
    records.extend(rv_turn(8, "t3", "high"));
    records.push(rv_user(9, "P3."));
    records.push(line(
        10,
        "response_item",
        json!({"type": "function_call", "name": "sleep", "arguments": "{}", "call_id": "c1"}),
    ));
    records.push(abort(11));
    records.extend(rv_turn(12, "t4", "high"));
    records.push(rv_user(13, "P4."));
    records.push(rv_said(14, "Fine.", Some("final_answer")));
    records.extend(rv_turn(15, "t5", "high"));
    records.push(rv_user(16, "P5."));
    records.push(abort(17));
    let source = jsonl(&records);
    let imported = passes(&source);
    let ends = ends_on(&imported);
    let on = |text: &str| {
        let node = witnessed_saying(&imported, text);
        ends.iter().filter(|(n, _)| *n == node).count()
    };
    assert_eq!(ends.len(), 2);
    assert_eq!(on("Thinking aloud."), 1);
    assert_eq!(on("P2."), 0);
    assert_eq!(on("P5."), 0);
    let facts = imported.fragment.facts();
    let claimed: Vec<Id> = find!(
        occurrence: Id,
        pattern!(facts, [{ ?occurrence @ v3::witness::turn_ends: v3::node::ends::INTERRUPTED }])
    )
    .collect();
    // X15, M2: each on its turn's last node: her commentary, his second
    // prompt, her call, his fifth prompt.
    assert_eq!(claimed.len(), 4);
    let on_turn_tail: Vec<Id> = turn_ends_on(&imported)
        .into_iter()
        .map(|(node, _)| node)
        .collect();
    let call = {
        let facts = imported.fragment.facts();
        let calls: Vec<Id> = find!(
            node: Id,
            pattern!(facts, [
                { ?node @ v3::node::holds: _?fact },
                { _?fact @ v3::fact::modality: v3::fact::modality::TOOL_CALL },
            ])
        )
        .collect();
        assert_eq!(calls.len(), 1);
        calls[0]
    };
    let mut expected = vec![
        witnessed_saying(&imported, "Thinking aloud."),
        witnessed_saying(&imported, "P2."),
        call,
        witnessed_saying(&imported, "P5."),
    ];
    expected.sort();
    let mut on_turn_tail = on_turn_tail;
    on_turn_tail.sort();
    assert_eq!(on_turn_tail, expected);
    let replaced = source.replacen("\"reason\":\"interrupted\"", "\"reason\":\"replaced\"", 1);
    assert_eq!(
        refused(&replaced).path,
        "{event_msg}.payload{turn_aborted}.reason"
    );
}

/// X6: a failed turn whose last item is hers ends there with `error`.
#[test]
fn x6_a_failed_turn_ends_on_her_last_node() {
    let mut records = Vec::new();
    rv_head(&mut records);
    records.push(rv_user(2, "Go."));
    records.push(rv_said(3, "Working.", Some("commentary")));
    records.push(line(
        4,
        "event_msg",
        json!({"type": "task_complete", "turn_id": "t1", "last_agent_message": null,
            "error": {"message": "stream disconnected", "codex_error_info": "other"}}),
    ));
    let imported = passes(&jsonl(&records));
    let working = witnessed_saying(&imported, "Working.");
    assert_eq!(ends_on(&imported), vec![(working, v3::node::ends::ERROR)]);
}

/// X7 (review CX-1): an empty sealed token takes its empty drop: the
/// reasoning is a body gap, never a zero-byte blob.
#[test]
fn x7_an_empty_token_is_no_blob() {
    let mut records = Vec::new();
    rv_head(&mut records);
    records.push(rv_user(2, "Q."));
    records.push(line(
        3,
        "response_item",
        json!({"type": "reasoning", "summary": [], "content": null, "encrypted_content": ""}),
    ));
    records.push(rv_said(4, "Done.", Some("final_answer")));
    let imported = passes(&jsonl(&records));
    let facts = imported.fragment.facts();
    let blobs = find!(
        fact: Id,
        pattern!(facts, [{ ?fact @ v3::fact::modality: v3::fact::modality::THINKING, v3::fact::blob: _?b }])
    )
    .count();
    let gaps = find!(
        fact: Id,
        pattern!(facts, [{ ?fact @ v3::fact::modality: v3::fact::modality::THINKING, v3::fact::absent: _?a }])
    )
    .count();
    assert_eq!((blobs, gaps), (0, 1));
}

/// C5 for Codex: items are known by their place, so two identical results
/// (identical heads) and a repeated text are two items: none recalls the
/// other. A header change re-presents them, and each copy recalls its own.
#[test]
fn identical_items_are_not_copies() {
    let mut records = Vec::new();
    rv_head(&mut records);
    records.push(rv_user(2, "Twice."));
    for (second, call) in [(3, "c1"), (5, "c2")] {
        records.push(line(
            second,
            "response_item",
            json!({"type": "function_call", "name": "ls", "arguments": "{}", "call_id": call}),
        ));
        records.push(line(
            second + 1,
            "response_item",
            json!({"type": "function_call_output", "call_id": call, "output": "a"}),
        ));
    }
    records.push(rv_said(7, "Same.", Some("final_answer")));
    let source = jsonl(&records);
    let imported = passes(&source);
    // Only the system prompt, re-presented under the first header, recalls.
    assert_eq!(annotated!(imported, v3::node::recalls).len(), 1);
    assert_eq!(annotated!(imported, v3::node::responds_to).len(), 2);
    let mut later = records.clone();
    later.extend(rv_turn(8, "t2", "low"));
    later.push(rv_user(9, "Again."));
    let imported = passes(&jsonl(&later));
    // Under the new header: the system prompt, the prompt, two calls, two
    // heads, two outputs and her answer, each recalling its own; and the
    // system prompt under the first header.
    assert_eq!(annotated!(imported, v3::node::recalls).len(), 10);
}

/// X10: world-state fields the codex source shows to be the client's
/// baseline (an environment's status, section fingerprints, the roster,
/// the model catalog) are machinery.
#[test]
fn x10_world_state_baseline_is_machinery() {
    let source = g3().replace(
        "\"apps_instructions\":false",
        "\"apps_instructions\":false,\
         \"collaboration_mode\":{\"instructions\":\"0beec7b5ea3f0fdbc95d0dd47f3c5bc275da8a33\",\
         \"mode\":\"default\",\"model\":\"gpt-5.5\"},\
         \"environments\":{\"environments\":{\"local\":{\"status\":\"available\"}}},\
         \"multi_agent_usage_hint\":\"62cdb7020ff920e5aa642c3d4066950dd1f01f4d\",\
         \"permissions\":{\"instructions\":\"0beec7b5ea3f0fdbc95d0dd47f3c5bc275da8a33\"}",
    );
    assert_ne!(source, g3());
    passes(&source);
}

// ---------------------------------------------------------------------------
// Round 4: M1, X8', X9', X11..X14, and the round-3 losslessness review's
// PARTLY fixtures (r3l_*), each of which failed against the round-3
// importer.

/// Why each sampling `node` holds stopped: the `ends` of each occurrence of
/// its witness, one sorted list per occurrence, sorted (M1).
fn ends_by_occurrence(imported: &Imported, node: Id) -> Vec<Vec<Id>> {
    let _ = ends_on(imported);
    let facts = imported.fragment.facts();
    let occurrences: BTreeSet<Id> = find!(
        occurrence: Id,
        pattern!(facts, [
            { _?witness @ v3::witness::projects_to: node },
            { ?occurrence @ v3::occurrence::of: _?witness },
        ])
    )
    .collect();
    let mut out: Vec<Vec<Id>> = occurrences
        .into_iter()
        .map(|occurrence| {
            let mut ends: Vec<Id> = find!(
                term: Id,
                pattern!(facts, [{ occurrence @ v3::witness::ends: ?term }])
            )
            .collect();
            ends.sort();
            ends
        })
        .collect();
    out.sort();
    out
}

fn blob_sizes(imported: &Imported, modality: Id) -> Vec<usize> {
    use triblespace::prelude::blobencodings::RawBytes;
    let facts = imported.fragment.facts();
    let handles: Vec<Inline<Handle<RawBytes>>> = find!(
        handle: Inline<Handle<RawBytes>>,
        pattern!(facts, [{ _?fact @ v3::fact::modality: modality, v3::fact::blob: ?handle }])
    )
    .collect();
    handles
        .into_iter()
        .map(|handle| {
            imported
                .blobs
                .get::<anybytes::Bytes, RawBytes>(handle)
                .unwrap()
                .len()
        })
        .collect()
}

fn body_gaps(imported: &Imported, modality: Id) -> usize {
    let facts = imported.fragment.facts();
    find!(
        fact: Id,
        pattern!(facts, [{ ?fact @ v3::fact::modality: modality, v3::fact::absent: v3::fact::absent::BODY }])
    )
    .count()
}

/// M1: two records that converge on one node (the same prompt and answer
/// after a rollback) keep each its own `ends`, on its own occurrence.
#[test]
fn m1_each_record_keeps_its_own_end() {
    let mut records = Vec::new();
    rv_head(&mut records);
    records.push(rv_user(2, "P."));
    records.push(rv_said(3, "Same.", Some("commentary")));
    records.push(line(
        4,
        "event_msg",
        json!({"type": "turn_aborted", "reason": "interrupted", "turn_id": "t1"}),
    ));
    records.push(line(
        5,
        "event_msg",
        json!({"type": "thread_rolled_back", "num_turns": 1}),
    ));
    records.extend(rv_turn(6, "t2", "high"));
    records.push(rv_user(7, "P."));
    records.push(rv_said(8, "Same.", Some("commentary")));
    records.push(line(
        9,
        "event_msg",
        json!({"type": "task_complete", "turn_id": "t2", "last_agent_message": null,
            "error": {"message": "stream disconnected", "codex_error_info": "other"}}),
    ));
    let imported = passes(&jsonl(&records));
    let same = nodes_saying(&imported, "Same.");
    assert_eq!(same.len(), 1, "one node, held by both records");
    // The abort's own `turn_ends` claim sits on the turn's last node too,
    // as an occurrence without `ends` (X15).
    let mut expected = vec![
        vec![],
        vec![v3::node::ends::ERROR],
        vec![v3::node::ends::INTERRUPTED],
    ];
    expected.sort();
    assert_eq!(ends_by_occurrence(&imported, same[0]), expected);
}

/// A fork (X8'): the child's own `session_meta` names `forked_from_id`, the
/// copied prefix keeps the ancestor's events but not its reasoning or
/// commentary, then the child's own turn.
fn x8_fork(prefix: &[Value], own: &[Value]) -> Vec<Value> {
    let child = "88888888-8888-4888-8888-888888888888";
    let parent = "99999999-9999-4999-8999-999999999999";
    let mut meta = rv_meta(0, child, "Sys.");
    meta["payload"]["forked_from_id"] = json!(parent);
    let mut records = vec![meta, rv_meta(0, parent, "Sys.")];
    records.extend(rv_turn(1, "t0", "high"));
    records.push(rv_user(2, "Parent asks."));
    records.extend(prefix.iter().cloned());
    records.push(line(
        3,
        "event_msg",
        json!({"type": "agent_message", "message": "Parent answers.", "phase": "final_answer"}),
    ));
    records.push(rv_said(3, "Parent answers.", Some("final_answer")));
    records.extend(rv_turn(4, "t1", "high"));
    records.push(rv_user(5, "Child task."));
    records.push(line(
        6,
        "response_item",
        json!({"type": "reasoning", "summary": [{"type": "summary_text", "text": "Child plan."}],
            "content": null, "encrypted_content": REASONING}),
    ));
    records.push(line(
        6,
        "event_msg",
        json!({"type": "agent_reasoning", "text": "Child plan."}),
    ));
    records.extend(own.iter().cloned());
    records.push(rv_said(7, "Done.", Some("final_answer")));
    records
}

#[test]
fn x8_a_forks_copied_prefix_keeps_events_of_removed_items() {
    let thought = line(
        2,
        "event_msg",
        json!({"type": "agent_reasoning", "text": "Parent thinks."}),
    );
    let comment = line(
        2,
        "event_msg",
        json!({"type": "agent_message", "message": "Parent comments.", "phase": "commentary"}),
    );
    let source = jsonl(&x8_fork(&[thought.clone(), comment.clone()], &[]));
    let imported = passes(&source);
    // The removed items are not imported: her window never held them.
    assert!(nodes_saying(&imported, "Parent comments.").is_empty());
    assert_eq!(glosses(&imported), vec!["Child plan.".to_owned()]);
    let rows =
        |source: &str, path: &str| walk_rows(source).contains(&(path.to_owned(), Kind::Mirror));
    let reasoning = "{event_msg}.payload{agent_reasoning}.text";
    let message = "{event_msg}.payload{agent_message}.message";
    // After the child's first item of a removed kind, a copy without its
    // original is the child's own and fails.
    let late = jsonl(&x8_fork(&[], &[thought.clone()]));
    assert_eq!(refused(&late).path, reasoning);
    assert!(rows(&late, reasoning));
    let late = jsonl(&x8_fork(&[], &[comment.clone()]));
    assert_eq!(refused(&late).path, message);
    assert!(rows(&late, message));
    // A unit that names no fork has no copied prefix.
    let mut records = x8_fork(&[thought.clone(), comment.clone()], &[]);
    records[0]["payload"]
        .as_object_mut()
        .unwrap()
        .remove("forked_from_id");
    let unforked = jsonl(&records);
    assert_eq!(refused(&unforked).path, reasoning);
    assert!(rows(&unforked, reasoning));
    // A final answer the fork filter keeps: its copy needs its original.
    let kept = line(
        2,
        "event_msg",
        json!({"type": "agent_message", "message": "Parent concludes.", "phase": "final_answer"}),
    );
    let kept = jsonl(&x8_fork(&[kept], &[]));
    assert_eq!(refused(&kept).path, message);
    assert!(rows(&kept, message));
    // A child that makes no item of a removed kind leaves the prefix
    // unbounded: the copy fails.
    let records: Vec<Value> = x8_fork(&[thought], &[])
        .into_iter()
        .filter(|record| {
            record["payload"]["type"] != json!("reasoning")
                && record["payload"]["text"] != json!("Child plan.")
        })
        .collect();
    let unbounded = jsonl(&records);
    assert_eq!(refused(&unbounded).path, reasoning);
    assert!(rows(&unbounded, reasoning));
}

/// X9': `guardian_history` copies the unit's own earlier items, in order;
/// nothing is imported for it.
#[test]
fn x9_guardian_history_mirrors_earlier_items() {
    let ask = json!({"type": "message", "role": "user",
        "content": [{"type": "input_text", "text": "A."}]});
    let call = json!({"type": "function_call", "name": "ls", "arguments": "{}", "call_id": "g1"});
    let output = json!({"type": "function_call_output", "call_id": "g1", "output": "listing"});
    let said = json!({"type": "message", "role": "assistant",
        "content": [{"type": "output_text", "text": "B."}]});
    let build = |guardian: Option<Value>| {
        let mut records = Vec::new();
        rv_head(&mut records);
        for (second, item) in [(2, &ask), (3, &call), (4, &output), (5, &said)] {
            records.push(line(second, "response_item", item.clone()));
        }
        let mut payload = json!({"message": "", "replacement_history": [
            ask.clone(),
            {"type": "compaction", "encrypted_content": COMPACTION}
        ]});
        if let Some(guardian) = guardian {
            payload["guardian_history"] = guardian;
        }
        records.push(line(6, "compacted", payload));
        records.push(rv_said(7, "E.", None));
        jsonl(&records)
    };
    let bare = import_ok(&build(None));
    for guardian in [
        json!([ask, call, output, said]),
        json!([call, said]),
        json!([]),
        json!(null),
    ] {
        let source = build(Some(guardian));
        let imported = passes(&source);
        let (a, b) = (imported.fragment.facts(), bare.fragment.facts());
        assert_eq!(a.len(), b.len(), "nothing is imported for the copies");
        assert_eq!(a.difference(b).len(), 0);
    }
    let path = "{compacted}.payload.guardian_history";
    let approved = json!({"type": "message", "role": "assistant",
        "content": [{"type": "output_text", "text": "Approved."}]});
    for guardian in [
        // Out of order.
        json!([said, ask]),
        // An item the unit does not hold.
        json!([ask, approved]),
        // A copy that differs from its item.
        json!([{"type": "function_call_output", "call_id": "g1", "output": "list"}]),
        // An item that is not earlier: her answer after the compaction.
        json!([{"type": "message", "role": "assistant",
            "content": [{"type": "output_text", "text": "E."}]}]),
    ] {
        let source = build(Some(guardian));
        assert_eq!(refused(&source).path, path);
        assert!(walk_rows(&source).contains(&(path.to_owned(), Kind::Mirror)));
    }
}

/// X9'': a guardian copy may hold a tool output as her live history did,
/// through `truncate_function_output_payload`'s budget-free arm: its output
/// list without the empty text parts (codex-rs utils/output-truncation,
/// `truncate_output_iter`). Any other difference, a budget's truncation
/// marker included, still fails, and each item is still copied once.
#[test]
fn x9pp_a_guardian_copy_may_drop_empty_output_text() {
    let call = json!({"type": "custom_tool_call", "name": "exec", "input": "ls", "call_id": "g2"});
    let part = |text: &str| json!({"type": "input_text", "text": text});
    let output = |parts: Vec<Value>| json!({"type": "custom_tool_call_output", "call_id": "g2", "output": parts});
    let full = output(vec![part(""), part("listing"), part("")]);
    let build = |guardian: Option<Value>| {
        let mut records = Vec::new();
        rv_head(&mut records);
        records.push(rv_user(2, "A."));
        records.push(line(3, "response_item", call.clone()));
        records.push(line(4, "response_item", full.clone()));
        let mut payload = json!({"message": "", "replacement_history": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "A."}]},
            {"type": "compaction", "encrypted_content": COMPACTION}
        ]});
        if let Some(guardian) = guardian {
            payload["guardian_history"] = guardian;
        }
        records.push(line(6, "compacted", payload));
        records.push(rv_said(7, "E.", None));
        jsonl(&records)
    };
    let bare = import_ok(&build(None));
    for guardian in [
        json!([call, output(vec![part("listing")])]),
        json!([call, full]),
    ] {
        let imported = passes(&build(Some(guardian)));
        let (a, b) = (imported.fragment.facts(), bare.fragment.facts());
        assert_eq!(a.len(), b.len(), "nothing is imported for the copies");
        assert_eq!(a.difference(b).len(), 0);
    }
    let path = "{compacted}.payload.guardian_history";
    for guardian in [
        // One empty part dropped, the other kept: no declared form.
        json!([output(vec![part("listing"), part("")])]),
        // A budget's truncation: not declared.
        json!([output(vec![part(
            "lis\u{2026}1 tokens truncated\u{2026}ng"
        )])]),
        // Both forms of one item: the item copied twice.
        json!([output(vec![part("listing")]), full]),
        // The text changed.
        json!([output(vec![part("listings")])]),
    ] {
        let source = build(Some(guardian));
        assert_eq!(refused(&source).path, path);
        assert!(walk_rows(&source).contains(&(path.to_owned(), Kind::Mirror)));
    }
}

/// X6, X11 (review r3l_cx_task_error): a failed turn ends with `error` on
/// her last item's occurrence, with no fact for the error; an error that
/// names no message fails.
#[test]
fn r3l_cx_task_error() {
    let done = |second: u32, error: Value| {
        line(
            second,
            "event_msg",
            json!({"type": "task_complete", "turn_id": "t", "last_agent_message": null,
                "error": error}),
        )
    };
    let build = |error: Value| {
        let mut records = Vec::new();
        rv_head(&mut records);
        records.push(rv_user(2, "Go."));
        records.push(line(
            3,
            "response_item",
            json!({"type": "reasoning", "summary": [], "content": null,
                "encrypted_content": REASONING}),
        ));
        records.push(line(
            4,
            "response_item",
            json!({"type": "function_call", "name": "ls", "arguments": "{}", "call_id": "e1"}),
        ));
        records.push(line(
            5,
            "response_item",
            json!({"type": "function_call_output", "call_id": "e1", "output": "out"}),
        ));
        records.push(done(6, error.clone()));
        records.extend(rv_turn(7, "t2", "high"));
        records.push(rv_user(8, "Again."));
        records.push(done(9, error));
        jsonl(&records)
    };
    let call_node = |imported: &Imported| {
        let facts = imported.fragment.facts();
        let calls: Vec<Id> = find!(
            node: Id,
            pattern!(facts, [
                { ?node @ v3::node::holds: _?fact },
                { _?fact @ v3::fact::modality: v3::fact::modality::TOOL_CALL },
            ])
        )
        .collect();
        calls
    };
    for error in [
        json!({"message": "boom", "codex_error_info": "other"}),
        json!({"message": ""}),
    ] {
        let imported = passes(&build(error));
        let calls = call_node(&imported);
        assert_eq!(calls.len(), 1);
        // On her call (the turn's last item of hers), not on his prompt.
        assert_eq!(ends_on(&imported), vec![(calls[0], v3::node::ends::ERROR)]);
        let facts = imported.fragment.facts();
        assert!(!exists!(
            (),
            pattern!(facts, [{ _?fact @ v3::fact::modality: v3::fact::modality::EVENT }])
        ));
        assert!(nodes_saying(&imported, "boom").is_empty());
    }
    let imported = passes(&build(json!(null)));
    assert!(ends_on(&imported).is_empty());
    let path = "{event_msg}.payload{task_complete}.error";
    for error in [json!({"codex_error_info": "other"}), json!("boom")] {
        let source = build(error);
        assert_eq!(refused(&source).path, path);
        assert!(walk_rows(&source).contains(&(path.to_owned(), Kind::Open)));
    }
}

/// X7, X12 (review r3l_cx_empty_tokens): every empty sealed token is a
/// body gap, never a zero-byte blob.
#[test]
fn r3l_cx_empty_tokens() {
    // Reasoning with a summary and an empty token: a gap, and its gloss.
    let mut records = Vec::new();
    rv_head(&mut records);
    records.push(rv_user(2, "Q."));
    records.push(line(
        3,
        "response_item",
        json!({"type": "reasoning", "summary": [{"type": "summary_text", "text": "Plan."}],
            "content": null, "encrypted_content": ""}),
    ));
    records.push(rv_said(4, "Done.", Some("final_answer")));
    let imported = passes(&jsonl(&records));
    assert!(blob_sizes(&imported, v3::fact::modality::THINKING).is_empty());
    assert_eq!(body_gaps(&imported, v3::fact::modality::THINKING), 1);
    assert_eq!(glosses(&imported), vec!["Plan.".to_owned()]);
    // Reasoning with an empty token inside a replacement history.
    let history = json!([
        {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "C."}]},
        {"type": "reasoning", "summary": [], "content": null, "encrypted_content": ""},
        {"type": "compaction", "encrypted_content": COMPACTION}
    ]);
    let imported = passes(&rv_compaction_source(history, &[rv_said(8, "E.", None)]));
    assert!(!blob_sizes(&imported, v3::fact::modality::THINKING).contains(&0));
    // A compaction item whose token is empty.
    let history = json!([
        {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "C."}]},
        {"type": "compaction", "encrypted_content": ""}
    ]);
    let imported = passes(&rv_compaction_source(history, &[rv_said(8, "E.", None)]));
    assert!(blob_sizes(&imported, v3::fact::modality::THINKING).is_empty());
    assert_eq!(body_gaps(&imported, v3::fact::modality::THINKING), 1);
    // An inter-agent message whose sealed content is empty.
    let mut records = Vec::new();
    rv_head(&mut records);
    records.push(rv_user(2, "Q."));
    records.push(line(
        3,
        "response_item",
        json!({"type": "agent_message", "author": "/root",
            "content": [{"type": "encrypted_content", "encrypted_content": ""}]}),
    ));
    records.push(rv_said(4, "Done.", Some("final_answer")));
    let imported = passes(&jsonl(&records));
    assert!(blob_sizes(&imported, v3::fact::modality::TEXT).is_empty());
    assert_eq!(body_gaps(&imported, v3::fact::modality::TEXT), 1);
}

/// X8', X13 (review r3l_cx_relayed_child_events): a relayed copy the unit
/// does not hold fails; a copy whose text the unit holds, but not in the
/// item after it, is matched by text alone and fails, so another agent's
/// message never sets her register.
#[test]
fn r3l_cx_relayed_child_events() {
    let build = |extra: Vec<Value>| {
        let mut records = Vec::new();
        rv_head(&mut records);
        records.push(rv_user(2, "Delegate."));
        records.push(line(
            3,
            "response_item",
            json!({"type": "function_call", "name": "spawn_agent",
                "arguments": "{\"task\":\"t\"}", "call_id": "s1"}),
        ));
        records.extend(extra);
        records.push(line(
            6,
            "response_item",
            json!({"type": "function_call_output", "call_id": "s1", "output": "child finished"}),
        ));
        records.push(rv_said(7, "Done.", None));
        jsonl(&records)
    };
    let message = build(vec![line(
        4,
        "event_msg",
        json!({"type": "agent_message", "message": "Child text.", "phase": "commentary"}),
    )]);
    assert_eq!(
        refused(&message).path,
        "{event_msg}.payload{agent_message}.message"
    );
    let reasoning = build(vec![line(
        4,
        "event_msg",
        json!({"type": "agent_reasoning", "text": "Child thinking."}),
    )]);
    assert_eq!(
        refused(&reasoning).path,
        "{event_msg}.payload{agent_reasoning}.text"
    );
    // The child's message happens to say what hers before it says.
    let mut records = Vec::new();
    rv_head(&mut records);
    records.push(rv_user(2, "Delegate."));
    records.push(rv_said(3, "Done.", None));
    records.push(line(
        4,
        "event_msg",
        json!({"type": "agent_message", "message": "Done.", "phase": "commentary"}),
    ));
    let coinciding = jsonl(&records);
    let phase = "{event_msg}.payload{agent_message}.phase";
    assert_eq!(refused(&coinciding).path, phase);
    assert!(walk_rows(&coinciding).contains(&(phase.to_owned(), Kind::Mirror)));
    // Its copy written before it, as codex writes it, is hers.
    let mut records = Vec::new();
    rv_head(&mut records);
    records.push(rv_user(2, "Delegate."));
    records.push(line(
        3,
        "event_msg",
        json!({"type": "agent_message", "message": "Done.", "phase": "commentary"}),
    ));
    records.push(line(
        3,
        "response_item",
        json!({"type": "reasoning", "summary": [], "content": null, "encrypted_content": REASONING}),
    ));
    records.push(rv_said(3, "Done.", None));
    let imported = passes(&jsonl(&records));
    assert_eq!(
        registers(&imported, witnessed_saying(&imported, "Done.")),
        vec![v3::fact::register::COMMENTARY]
    );
}

/// X14: a text part without its text fails at its path, never skipped.
#[test]
fn x14_a_text_part_without_text_fails() {
    let mut cases: Vec<(Value, &str)> = vec![
        (
            json!({"type": "message", "role": "user", "content": [{"type": "input_text"}]}),
            "{response_item}.payload{message}.content[]{input_text}.text",
        ),
        (
            json!({"type": "message", "role": "developer", "content": [{"type": "input_text"}]}),
            "{response_item}.payload{message}.content[]{input_text}.text",
        ),
        (
            json!({"type": "message", "role": "assistant", "content": [{"type": "output_text"}]}),
            "{response_item}.payload{message}.content[]{output_text}.text",
        ),
        (
            json!({"type": "reasoning", "summary": [{"type": "summary_text"}], "content": null,
                "encrypted_content": REASONING}),
            "{response_item}.payload{reasoning}.summary[]{summary_text}.text",
        ),
        (
            json!({"type": "agent_message", "author": "/root",
                "content": [{"type": "input_text"}]}),
            "{response_item}.payload{agent_message}.content[]{input_text}.text",
        ),
        // A text that is no string: the table names no class for it.
        (
            json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": 7}]}),
            "{response_item}.payload{message}{user}.content[]{input_text}.text",
        ),
    ];
    cases.push((
        json!({"type": "function_call_output", "call_id": "x1",
            "output": [{"type": "input_text"}]}),
        "{response_item}.payload{function_call_output}.output[]{input_text}.text",
    ));
    for (item, path) in cases {
        let mut records = Vec::new();
        rv_head(&mut records);
        records.push(rv_user(2, "Q."));
        records.push(line(
            3,
            "response_item",
            json!({"type": "function_call", "name": "ls", "arguments": "{}", "call_id": "x1"}),
        ));
        records.push(line(4, "response_item", item));
        let source = jsonl(&records);
        assert_eq!(refused(&source).path, path);
    }
}

// ---------------------------------------------------------------------------
// Round 5: M2, X15, X13', each of which failed against the round-4
// importer.

/// The `turn_ends` claims of the import: `(node, term)` by the node the
/// claiming occurrence's witness projects to.
fn turn_ends_on(imported: &Imported) -> Vec<(Id, Id)> {
    let facts = imported.fragment.facts();
    let mut found: Vec<(Id, Id)> = find!(
        (node: Id, term: Id),
        pattern!(facts, [
            { _?witness @ v3::witness::projects_to: ?node },
            { _?occurrence @ v3::occurrence::of: _?witness, v3::witness::turn_ends: ?term },
        ])
    )
    .collect();
    found.sort();
    found
}

/// M2: no prefix of a rollout (a backup snapshot cut anywhere) adds an
/// occurrence, or any trible at all, to the full rollout's import. Why a
/// sampling or a turn ended is known only from a later record (the abort,
/// the erred completion), so it annotates the occurrence and is no part of
/// its id, and a turn's end is claimed on the turn's own last node, which
/// the snapshot already holds (X15). The round-4 losslessness review's
/// `r4l_cx_backup_overlap`, kept as a permanent test.
#[test]
fn m2_every_prefix_adds_no_occurrence_to_the_full_import() {
    let mut records = Vec::new();
    rv_head(&mut records);
    records.push(rv_user(2, "P1."));
    records.push(line(
        3,
        "response_item",
        json!({"type": "reasoning", "summary": [{"type": "summary_text", "text": "Plan."}],
            "content": null, "encrypted_content": REASONING}),
    ));
    records.push(line(
        3,
        "response_item",
        json!({"type": "function_call", "name": "ls", "arguments": "{}", "call_id": "b1"}),
    ));
    records.push(line(
        4,
        "response_item",
        json!({"type": "function_call_output", "call_id": "b1", "output": "o"}),
    ));
    records.push(line(
        5,
        "event_msg",
        json!({"type": "agent_message", "message": "R1.", "phase": "commentary"}),
    ));
    records.push(rv_said(5, "R1.", None));
    records.push(line(
        6,
        "event_msg",
        json!({"type": "turn_aborted", "reason": "interrupted", "turn_id": "t1"}),
    ));
    records.extend(rv_turn(7, "t2", "xhigh"));
    records.push(rv_user(8, "P2."));
    records.push(rv_said(9, "R2.", Some("final_answer")));
    records.push(line(
        10,
        "event_msg",
        json!({"type": "task_complete", "turn_id": "t2", "last_agent_message": "R2.",
            "error": {"message": "boom", "codex_error_info": "other"}}),
    ));
    records.push(line(
        11,
        "compacted",
        json!({"message": "", "replacement_history": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "P2."}]},
            {"type": "compaction", "encrypted_content": COMPACTION}]}),
    ));
    records.extend(rv_turn(12, "t3", "high"));
    records.push(rv_user(13, "P3."));
    records.push(line(
        14,
        "event_msg",
        json!({"type": "task_complete", "turn_id": "t3", "last_agent_message": null,
            "error": {"message": "boom", "codex_error_info": "other"}}),
    ));
    records.extend(rv_turn(15, "t4", "high"));
    records.push(rv_user(16, "P4."));
    records.push(rv_said(17, "R4.", Some("final_answer")));
    let full = jsonl(&records);
    let live = passes(&full);
    let r1 = witnessed_saying(&live, "R1.");
    let p3 = witnessed_saying(&live, "P3.");
    assert_eq!(turn_ends_on(&live), {
        let mut expected = vec![
            (r1, v3::node::ends::INTERRUPTED),
            (p3, v3::node::ends::ERROR),
        ];
        expected.sort();
        expected
    });
    let mut cuts = 0;
    for k in 1..records.len() {
        let prefix = jsonl(&records[..k]);
        let Ok(fragment) = import(prefix.as_bytes()) else {
            continue;
        };
        cuts += 1;
        let extra = fragment.facts().difference(live.fragment.facts());
        let facts = &extra;
        let added = find!(
            occurrence: Id,
            pattern!(facts, [{ ?occurrence @ v3::occurrence::of: _?witness }])
        )
        .count();
        assert_eq!(added, 0, "the prefix of {k} records adds an occurrence");
        assert_eq!(extra.len(), 0, "the prefix of {k} records adds tribles");
        let mut union = live.fragment.clone();
        union += fragment;
        let blobs = union.blobs_mut().snapshot().unwrap();
        let union = Imported {
            fragment: union,
            blobs,
        };
        let report = gate(&full, &union);
        assert!(
            report.passed(),
            "the union with the prefix of {k}: {report}"
        );
    }
    assert!(cuts > 10, "only {cuts} prefixes import");
}

/// M2: two records whose own claims coincide (one node, the same second)
/// converge on one occurrence; when they ended differently, which stopped
/// which way would be lost, so the unit fails. Ending the same way, they
/// are one occurrence, as before.
#[test]
fn m2_one_claim_set_that_ends_two_ways_fails() {
    let build = |first: Value, second: Value| {
        let mut records = Vec::new();
        rv_head(&mut records);
        records.push(rv_user(3, "P."));
        records.push(rv_said(3, "Same.", Some("commentary")));
        records.push(first);
        records.push(line(
            3,
            "event_msg",
            json!({"type": "thread_rolled_back", "num_turns": 1}),
        ));
        records.push(line(
            3,
            "event_msg",
            json!({"type": "task_started", "turn_id": "t2"}),
        ));
        records.push(rv_user(3, "P."));
        records.push(rv_said(3, "Same.", Some("commentary")));
        records.push(second);
        jsonl(&records)
    };
    let abort = json!({"timestamp": stamp(3), "type": "event_msg",
        "payload": {"type": "turn_aborted", "reason": "interrupted", "turn_id": "t"}});
    let erred = json!({"timestamp": stamp(3), "type": "event_msg",
        "payload": {"type": "task_complete", "turn_id": "t", "last_agent_message": null,
            "error": {"message": "stream disconnected", "codex_error_info": "other"}}});
    assert_eq!(
        refused(&build(abort.clone(), erred)).path,
        "{event_msg}.payload"
    );
    let imported = passes(&build(abort.clone(), abort));
    let same = nodes_saying(&imported, "Same.");
    assert_eq!(same.len(), 1);
    assert_eq!(
        ends_by_occurrence(&imported, same[0]),
        vec![vec![], vec![v3::node::ends::INTERRUPTED]]
    );
}

/// X15 (review r4l_cx_task_error, refuted in round 4): a failed turn is
/// never lost. An erred turn in which she produced nothing claims
/// `turn_ends: error` on the turn's last node, or on the next header when
/// the turn holds no node, and the gate tells it from a clean turn. The
/// error's `message` must be a string.
#[test]
fn x15_a_failed_turn_is_never_lost() {
    let done = |second: u32, error: Value| {
        line(
            second,
            "event_msg",
            json!({"type": "task_complete", "turn_id": "t", "last_agent_message": null,
                "error": error}),
        )
    };
    let build = |error: Value, prompt: bool| {
        let mut records = Vec::new();
        rv_head(&mut records);
        if prompt {
            records.push(rv_user(2, "Go."));
        }
        records.push(done(4, error));
        records.extend(rv_turn(5, "t2", "xhigh"));
        records.push(rv_user(6, "Next."));
        records.push(rv_said(7, "Ok.", Some("final_answer")));
        jsonl(&records)
    };
    let failed = json!({"message": "boom", "codex_error_info": "other"});
    // Nothing of hers: the claim is on the turn's last node, his prompt.
    let erred = build(failed.clone(), true);
    let clean = build(json!(null), true);
    let imported = passes(&erred);
    let go = witnessed_saying(&imported, "Go.");
    assert!(ends_on(&imported).is_empty());
    assert_eq!(turn_ends_on(&imported), vec![(go, v3::node::ends::ERROR)]);
    let report = gate(&clean, &imported);
    assert!(
        !report.passed(),
        "the gate took an erred import for a clean turn"
    );
    let report = gate(&erred, &passes(&clean));
    assert!(
        !report.passed(),
        "the gate took a clean import for an erred turn"
    );
    // A turn that holds no node: the claim is on the next header.
    let headless = build(failed.clone(), false);
    let imported = passes(&headless);
    let facts = imported.fragment.facts();
    let claimed: Vec<Id> = find!(
        fact: Id,
        pattern!(facts, [
            { _?witness @ v3::witness::projects_to: _?node },
            { _?occurrence @ v3::occurrence::of: _?witness, v3::witness::turn_ends: v3::node::ends::ERROR },
            { _?node @ v3::node::holds: ?fact },
            { ?fact @ v3::fact::effort: v3::fact::effort::XHIGH },
        ])
    )
    .collect();
    assert_eq!(claimed.len(), 1, "on the next header");
    // With something of hers the turn ends on her item, and claims nothing
    // more (X6).
    let mut records = Vec::new();
    rv_head(&mut records);
    records.push(rv_user(2, "Go."));
    records.push(rv_said(3, "Working.", Some("commentary")));
    records.push(done(4, failed));
    let imported = passes(&jsonl(&records));
    let working = witnessed_saying(&imported, "Working.");
    assert_eq!(ends_on(&imported), vec![(working, v3::node::ends::ERROR)]);
    assert!(turn_ends_on(&imported).is_empty());
    // The message is a string, or the unit fails.
    let path = "{event_msg}.payload{task_complete}.error";
    for error in [
        json!({"message": null, "codex_error_info": "other"}),
        json!({"message": 7}),
        json!({"message": ["boom"]}),
        json!({"message": {"text": "boom"}}),
        json!({"message": true}),
    ] {
        assert_eq!(refused(&build(error.clone(), true)).path, path);
        assert_eq!(refused(&build(error, false)).path, path);
    }
}

/// X13' (review r4l_cx_followups): a phase copy whose message only a
/// compaction's replacement history holds has no structural match, and
/// fails; the same copy before the message itself still sets its register.
#[test]
fn x13p_a_copy_of_a_message_only_a_replacement_holds_fails() {
    let build = |phase: Option<&str>| {
        let mut records = Vec::new();
        rv_head(&mut records);
        records.push(rv_user(2, "Q."));
        records.push(rv_said(3, "Done.", None));
        let mut event = json!({"type": "agent_message", "message": "Kept."});
        if let Some(phase) = phase {
            event["phase"] = json!(phase);
        }
        records.push(line(4, "event_msg", event));
        records.push(line(
            5,
            "compacted",
            json!({"message": "", "replacement_history": [
                {"type": "message", "role": "assistant",
                    "content": [{"type": "output_text", "text": "Kept."}]},
                {"type": "compaction", "encrypted_content": COMPACTION}
            ]}),
        ));
        records.push(rv_said(6, "After.", None));
        jsonl(&records)
    };
    let path = "{event_msg}.payload{agent_message}.phase";
    assert_eq!(refused(&build(Some("commentary"))).path, path);
    assert_eq!(refused(&build(Some("final_answer"))).path, path);
    // No phase to supply: nothing is lost, and the copy is a mirror.
    passes(&build(None));
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
    let mut skipped = 0;
    let mut first_failures: BTreeMap<(String, &'static str), usize> = BTreeMap::new();
    let mut walk_failures: BTreeMap<(String, String), usize> = BTreeMap::new();
    let mut gate_failures: BTreeMap<(String, String), usize> = BTreeMap::new();
    let mut failing_files: Vec<String> = Vec::new();
    let mut by_month: BTreeMap<String, [usize; 3]> = BTreeMap::new();
    for file in &files {
        if std::fs::metadata(file).is_ok_and(|meta| meta.len() > 400 * 1024 * 1024) {
            skipped += 1;
            continue;
        }
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
                    .entry((gate::vocabulary(&TABLE, &failure.path), failure.reason))
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
                    let paths: std::collections::BTreeSet<String> = report
                        .rows
                        .iter()
                        .map(|row| format!("{} {}", row.path, row.kind))
                        .collect();
                    let paths: Vec<String> = paths.into_iter().collect();
                    failing_files.push(format!("{}\t{}", gate::file_path(file), paths.join("; ")));
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
        "files {} skipped-over-400MB {} unread {} passed {} import-failed {} gate-failed {}",
        files.len(),
        skipped,
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
    println!("-- files that import and fail the gate");
    for file in &failing_files {
        println!("{file}");
    }
}

// ---------------------------------------------------------------------------
// The round-5 losslessness review's Codex fixtures (`r5l_cx_*`), kept as
// permanent tests of rulings X16 to X19, and of the items that held.

fn r5_try(source: &str) -> Option<Imported> {
    let mut fragment = import(source.as_bytes()).ok()?;
    let blobs = fragment.blobs_mut().snapshot().unwrap();
    Some(Imported { fragment, blobs })
}

fn r5_outcome(source: &str) -> String {
    match import(source.as_bytes()) {
        Err(f) => format!("refused@{}", f.path),
        Ok(mut fragment) => {
            let blobs = fragment.blobs_mut().snapshot().unwrap();
            let imported = Imported { fragment, blobs };
            format!("imported gate={}", gate(source, &imported).passed())
        }
    }
}

/// Whether two sources import to different TribleSets (both must import).
fn r5_differ(a: &str, b: &str) -> bool {
    match (import(a.as_bytes()), import(b.as_bytes())) {
        (Ok(fa), Ok(fb)) => fa.facts() != fb.facts(),
        (Err(f), _) | (_, Err(f)) => panic!("refused@{}", f.path),
    }
}

fn r5_label(imported: &Imported, node: Id) -> String {
    use triblespace::prelude::blobencodings::RawBytes;
    let facts = imported.fragment.facts();
    let payload: Vec<Inline<Handle<UTF8String>>> = find!(
        p: Inline<Handle<UTF8String>>,
        pattern!(facts, [{ node @ v3::node::holds: _?f }, { _?f @ v3::fact::payload: ?p }])
    )
    .collect();
    let name: Vec<Inline<Handle<UTF8String>>> = find!(
        p: Inline<Handle<UTF8String>>,
        pattern!(facts, [{ node @ v3::node::holds: _?f }, { _?f @ v3::fact::tool_name: ?p }])
    )
    .collect();
    let blob: Vec<Inline<Handle<RawBytes>>> = find!(
        p: Inline<Handle<RawBytes>>,
        pattern!(facts, [{ node @ v3::node::holds: _?f }, { _?f @ v3::fact::blob: ?p }])
    )
    .collect();
    let effort = exists!(
        (),
        pattern!(facts, [{ node @ v3::node::holds: _?f }, { _?f @ v3::fact::effort: _?e }])
    );
    let mut out = String::new();
    if effort {
        out += "<header>";
    }
    if let Some(n) = name.first() {
        out += &format!("[{}]", text_of(imported, *n));
    }
    if let Some(p) = payload.first() {
        out += &format!("{:?}", text_of(imported, *p));
    }
    if let Some(b) = blob.first() {
        let bytes = imported.blobs.get::<anybytes::Bytes, RawBytes>(*b).unwrap();
        out += &format!("<blob {}B>", bytes.len());
    }
    if out.is_empty() {
        out = "<other>".into();
    }
    out
}

fn r5_claims(imported: &Imported) -> String {
    let facts = imported.fragment.facts();
    let mut ends: Vec<String> = find!(
        (node: Id, term: Id),
        pattern!(facts, [
            { _?w @ v3::witness::projects_to: ?node },
            { _?o @ v3::occurrence::of: _?w, v3::witness::ends: ?term },
        ])
    )
    .map(|(n, t)| format!("ends:{}@{}", r5_term(t), r5_label(imported, n)))
    .collect();
    let mut turn: Vec<String> = find!(
        (node: Id, term: Id),
        pattern!(facts, [
            { _?w @ v3::witness::projects_to: ?node },
            { _?o @ v3::occurrence::of: _?w, v3::witness::turn_ends: ?term },
        ])
    )
    .map(|(n, t)| format!("turn_ends:{}@{}", r5_term(t), r5_label(imported, n)))
    .collect();
    ends.sort();
    turn.sort();
    ends.extend(turn);
    ends.join(" ")
}

fn r5_term(t: Id) -> &'static str {
    use v3::node::ends::*;
    match t {
        x if x == INTERRUPTED => "interrupted",
        x if x == ERROR => "error",
        _ => "other",
    }
}

/// M2, X19: every prefix of `records` (a backup snapshot cut there) either
/// fails or adds nothing to the full import.
fn r5_prefixes(records: &[Value]) {
    let full = jsonl(records);
    let live = passes(&full);
    for k in 1..records.len() {
        let prefix = jsonl(&records[..k]);
        if let Ok(fragment) = import(prefix.as_bytes()) {
            let extra = fragment.facts().difference(live.fragment.facts());
            assert_eq!(
                extra.len(),
                0,
                "the prefix cut after {k} records adds to the full import"
            );
        }
    }
}

/// X15, M2 and X19 (review `r5l_cx_x15_and_prefixes`): an erred turn with
/// nothing of hers, with and without a node, and every prefix of it. A
/// claim waiting for the next node never lands on the previous turn's last
/// node when a cut falls before that node: a unit whose last records'
/// claims wait for a node it does not reach fails.
#[test]
fn r5l_cx_x15_and_prefixes() {
    let erred = |s: u32, message: Value| {
        line(
            s,
            "event_msg",
            json!({"type": "task_complete", "turn_id": "t", "last_agent_message": null,
            "error": {"message": message, "codex_error_info": "other"}}),
        )
    };
    let clean = |s: u32| {
        line(
            s,
            "event_msg",
            json!({"type": "task_complete", "turn_id": "t", "last_agent_message": null,
                "error": null}),
        )
    };
    let abort = |s: u32| {
        line(
            s,
            "event_msg",
            json!({"type": "turn_aborted", "reason": "interrupted", "turn_id": "t"}),
        )
    };
    // With his prompt.
    let build = |end: Value| {
        let mut r = Vec::new();
        rv_head(&mut r);
        r.push(rv_user(2, "P1."));
        r.push(rv_said(3, "R1.", Some("final_answer")));
        r.push(clean(4));
        r.extend(rv_turn(5, "t2", "xhigh"));
        r.push(rv_user(6, "P2."));
        r.push(end);
        r.extend(rv_turn(8, "t3", "low"));
        r.push(rv_user(9, "P3."));
        r.push(rv_said(10, "R3.", Some("final_answer")));
        r
    };
    let e = jsonl(&build(erred(7, json!("boom"))));
    let c = jsonl(&build(clean(7)));
    let ie = passes(&e);
    assert_eq!(r5_claims(&ie), "turn_ends:error@\"P2.\"");
    assert!(!gate(&c, &ie).passed());
    assert!(r5_differ(&e, &c));
    assert_eq!(
        r5_outcome(&jsonl(&build(erred(7, json!(""))))),
        "imported gate=true"
    );
    for m in [json!(null), json!(7)] {
        assert_eq!(
            r5_outcome(&jsonl(&build(erred(7, m)))),
            "refused@{event_msg}.payload{task_complete}.error"
        );
    }
    r5_prefixes(&build(erred(7, json!("boom"))));
    // Headless: the turn holds no node (task_started, then the end).
    let headless = |end: Value| {
        let mut r = Vec::new();
        rv_head(&mut r);
        r.push(rv_user(2, "P1."));
        r.push(rv_said(3, "R1.", Some("final_answer")));
        r.push(clean(4));
        r.push(line(
            5,
            "event_msg",
            json!({"type": "task_started", "turn_id": "t2"}),
        ));
        r.push(end);
        r.extend(rv_turn(8, "t3", "low"));
        r.push(rv_user(9, "P3."));
        r.push(rv_said(10, "R3.", Some("final_answer")));
        r
    };
    let he = jsonl(&headless(erred(7, json!("boom"))));
    let ih = passes(&he);
    assert_eq!(r5_claims(&ih), "turn_ends:error@<header>");
    assert!(r5_differ(&he, &jsonl(&headless(clean(7)))));
    r5_prefixes(&headless(erred(7, json!("boom"))));
    r5_prefixes(&headless(abort(7)));
    // A headless end at the end of the file: its claim's node lies beyond
    // the cut, so the unit fails rather than give it to R1.
    let cut = &headless(abort(7))[..8];
    assert_eq!(
        r5_outcome(&jsonl(cut)),
        "refused@{event_msg}.payload{turn_aborted}.reason"
    );
    // An effortless header holds no node; its claims go to the next node.
    let mut r = Vec::new();
    rv_head(&mut r);
    r.push(rv_user(2, "P1."));
    r.push(rv_said(3, "R1.", Some("final_answer")));
    r.push(line(
        5,
        "event_msg",
        json!({"type": "task_started", "turn_id": "t2"}),
    ));
    r.push(line(
        5,
        "turn_context",
        json!({"model": "gpt-5.4", "summary": "auto", "turn_id": "t2"}),
    ));
    r.push(rv_user(6, "P2."));
    r.push(rv_said(7, "R2.", Some("final_answer")));
    r5_prefixes(&r);
    assert_eq!(
        r5_outcome(&jsonl(&r[..7])),
        "refused@{turn_context}.payload"
    );
}

/// X17 (review `r5l_cx_unstarted_abort`): in a rollout that marks no turn
/// with `task_started`, a turn starts at his prompt after her answer. An
/// abort after that prompt, before she produced anything, carries no
/// `ends` on her previous answer (X5); its `turn_ends` is the prompt's.
#[test]
fn r5l_cx_unstarted_abort() {
    let prompt = |s: u32, text: &str| {
        [
            rv_user(s, text),
            line(
                s,
                "event_msg",
                json!({"type": "user_message", "message": text, "images": []}),
            ),
        ]
    };
    let aborted = |s: u32| {
        line(
            s,
            "event_msg",
            json!({"type": "turn_aborted", "reason": "interrupted"}),
        )
    };
    let mut r = vec![
        rv_meta(0, "77777777-7777-4777-8777-777777777777", "Sys."),
        line(
            1,
            "turn_context",
            json!({"model": "gpt-5", "effort": "high", "summary": "auto"}),
        ),
    ];
    r.extend(prompt(2, "P1."));
    r.push(rv_said(3, "R1.", None));
    r.extend(prompt(4, "P2."));
    r.push(aborted(5));
    r.extend(prompt(6, "P3."));
    r.push(rv_said(7, "R3.", None));
    let i = passes(&jsonl(&r));
    assert_eq!(r5_claims(&i), "turn_ends:interrupted@\"P2.\"");
    // Control: the abort right after her answer ends it.
    let mut r2 = r[..5].to_vec();
    r2.push(aborted(5));
    r2.extend(prompt(6, "P3."));
    r2.push(rv_said(7, "R3.", None));
    let i = passes(&jsonl(&r2));
    assert_eq!(
        r5_claims(&i),
        "ends:interrupted@\"R1.\" turn_ends:interrupted@\"R1.\""
    );
    // Two aborts: after her answer, then after a new prompt.
    let mut r3 = r2[..6].to_vec();
    r3.extend(prompt(6, "P2b."));
    r3.push(aborted(6));
    r3.extend(prompt(8, "P3."));
    r3.push(rv_said(9, "R3.", None));
    let i = passes(&jsonl(&r3));
    assert_eq!(
        r5_claims(&i),
        "ends:interrupted@\"R1.\" turn_ends:interrupted@\"P2b.\" \
         turn_ends:interrupted@\"R1.\""
    );
    // Several prompts in a row are one turn's input: the abort after them
    // is the last prompt's, and R1 keeps no end.
    let mut r4 = r[..7].to_vec();
    r4.extend(prompt(4, "P2c."));
    r4.push(aborted(5));
    r4.extend(prompt(6, "P3."));
    r4.push(rv_said(7, "R3.", None));
    let i = passes(&jsonl(&r4));
    assert_eq!(r5_claims(&i), "turn_ends:interrupted@\"P2c.\"");
}

/// X13' (review `r5l_cx_x13p`): a phase copy whose message a compaction
/// installs. Held as ruled; kept as a permanent test.
#[test]
fn r5l_cx_x13p() {
    let ev = |s: u32, text: &str| {
        line(
            s,
            "event_msg",
            json!({"type": "agent_message", "message": text, "phase": "commentary"}),
        )
    };
    let compacted = |s: u32, text: &str| {
        line(
            s,
            "compacted",
            json!({"message": "", "replacement_history": [
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text}]},
            {"type": "compaction", "encrypted_content": COMPACTION}]}),
        )
    };
    let head = || {
        let mut r = Vec::new();
        rv_head(&mut r);
        r.push(rv_user(2, "Q."));
        r
    };
    let copy = "refused@{event_msg}.payload{agent_message}.phase";
    let mut a = head();
    a.extend([
        ev(3, "Kept."),
        compacted(4, "Kept."),
        rv_said(5, "After.", None),
    ]);
    assert_eq!(r5_outcome(&jsonl(&a)), copy);
    let mut b = head();
    b.extend([
        ev(3, "Kept."),
        rv_said(3, "Kept.", None),
        compacted(4, "Kept."),
        rv_said(5, "After.", None),
    ]);
    let i = passes(&jsonl(&b));
    let set: Vec<bool> = nodes_saying(&i, "Kept.")
        .into_iter()
        .filter(|n| witnessed(&i, *n))
        .map(|n| registers(&i, n) == vec![v3::fact::register::COMMENTARY])
        .collect();
    assert_eq!(set.iter().filter(|set| **set).count(), 1);
    let mut c = head();
    c.extend([
        rv_said(3, "Kept.", None),
        compacted(4, "Kept."),
        ev(5, "Kept."),
        rv_said(5, "Kept.", None),
    ]);
    assert_eq!(r5_outcome(&jsonl(&c)), "imported gate=true");
    let mut d = head();
    d.extend([compacted(4, "Kept."), ev(5, "Kept.")]);
    assert_eq!(r5_outcome(&jsonl(&d)), copy);
}

/// The full payloads the record kept beside her window (X18).
fn untruncated(imported: &Imported) -> Vec<String> {
    let facts = imported.fragment.facts();
    let found: Vec<Inline<Handle<UTF8String>>> = find!(
        text: Inline<Handle<UTF8String>>,
        pattern!(facts, [{ _?occurrence @ v3::witness::untruncated: ?text }])
    )
    .collect();
    let mut texts: Vec<String> = found.into_iter().map(|t| text_of(imported, t)).collect();
    texts.sort();
    texts.dedup();
    texts
}

/// The window before `Done.`, labelled.
fn r5_window(imported: &Imported) -> String {
    let done = witnessed_saying(imported, "Done.");
    let mut v = vec![];
    let mut cur = previous(imported, done);
    while let Some(n) = cur {
        v.push(r5_label(imported, n));
        cur = previous(imported, n);
    }
    v.reverse();
    v.join(" , ")
}

/// X18 (review `r5l_cx_budget`): her window holds a tool output as codex's
/// history truncated it under the recorded budget, and the full payload is
/// a claim of the record beside it. A turn whose header records no budget
/// was written by a client that records none (codex 0.78 to 0.132 write it
/// on every header), so a budget is never carried into it. An unparsable
/// budget fails.
#[test]
fn r5l_cx_budget() {
    let build = |second_policy: Option<Value>, output: Value, metadata: Option<Value>| {
        let mut r = vec![rv_meta(0, "77777777-7777-4777-8777-777777777777", "Sys.")];
        r.push(line(
            1,
            "event_msg",
            json!({"type": "task_started", "turn_id": "t1"}),
        ));
        r.push(line(
            1,
            "turn_context",
            json!({"model": "gpt-5.5", "effort": "high", "summary": "auto", "turn_id": "t1",
            "truncation_policy": {"mode": "bytes", "limit": 10}}),
        ));
        r.push(rv_user(2, "Go."));
        r.push(rv_said(3, "Ok.", Some("final_answer")));
        r.push(line(
            4,
            "event_msg",
            json!({"type": "task_started", "turn_id": "t2"}),
        ));
        let mut ctx =
            json!({"model": "gpt-5.5", "effort": "high", "summary": "auto", "turn_id": "t2"});
        if let Some(p) = second_policy {
            ctx["truncation_policy"] = p;
        }
        r.push(line(4, "turn_context", ctx));
        r.push(rv_user(5, "Again."));
        r.push(line(
            6,
            "response_item",
            json!({"type": "custom_tool_call", "name": "exec", "input": "ls", "call_id": "o1"}),
        ));
        let mut out = line(
            7,
            "response_item",
            json!({"type": "custom_tool_call_output", "call_id": "o1", "output": output}),
        );
        if let Some(metadata) = metadata {
            out["metadata"] = metadata;
        }
        r.push(out);
        r.push(rv_said(8, "Done.", Some("final_answer")));
        jsonl(&r)
    };
    let x = |n: usize| json!("x".repeat(n));
    let policy = "refused@{turn_context}.payload.truncation_policy";
    let output = "refused@{response_item}.payload{custom_tool_call_output}.output";
    // No budget recorded in the second turn: nothing is carried into it.
    let none = build(None, x(13), None);
    let i = passes(&none);
    assert!(untruncated(&i).is_empty());
    assert_eq!(nodes_saying(&i, &"x".repeat(13)).len(), 1);
    assert_eq!(
        r5_outcome(&build(
            Some(json!({"mode": "bytes", "limit": 100})),
            x(13),
            None
        )),
        "imported gate=true"
    );
    for bad in [
        json!(null),
        json!({"mode": "lines", "limit": 1}),
        json!({"mode": "bytes", "limit": 1.5}),
        json!({"mode": "bytes", "limit": "1"}),
        json!({"mode": "bytes", "limit": null}),
        json!({"mode": "bytes", "limit": -1}),
        json!({"mode": "bytes"}),
        json!({"mode": "bytes", "limit": 1, "extra": 1}),
    ] {
        assert_eq!(r5_outcome(&build(Some(bad), x(13000), None)), policy);
    }
    // Bytes 10 allow 12 (`with_serialization_allowance`): 13 bytes keep 6
    // and 6, with a character count.
    let bytes = |n: usize| Some(json!({"mode": "bytes", "limit": n}));
    let source = build(bytes(10), x(13), None);
    let i = passes(&source);
    let shown = "xxxxxx\u{2026}1 chars truncated\u{2026}xxxxxx";
    assert_eq!(witnessed(&i, witnessed_saying(&i, shown)), true);
    assert!(nodes_saying(&i, &"x".repeat(13)).is_empty());
    assert_eq!(untruncated(&i), vec!["x".repeat(13)]);
    // The gate tells the truncated window from the full one.
    let full = import_ok(&build(bytes(100), x(13), None));
    assert!(!gate(&source, &full).passed());
    assert!(!gate(&build(bytes(100), x(13), None), &i).passed());
    // Tokens 2 allow 3, 12 bytes; the cost is in UTF-8 bytes
    // (`approx_token_count`), and the cut keeps whole characters.
    let mb = build(
        Some(json!({"mode": "tokens", "limit": 2})),
        json!("\u{e9}".repeat(7)),
        None,
    );
    let i = passes(&mb);
    witnessed_saying(
        &i,
        "\u{e9}\u{e9}\u{e9}\u{2026}1 tokens truncated\u{2026}\u{e9}\u{e9}\u{e9}",
    );
    assert_eq!(untruncated(&i), vec!["\u{e9}".repeat(7)]);
    // A budget of zero shows only the marker.
    let i = passes(&build(bytes(0), x(5), None));
    witnessed_saying(&i, "\u{2026}5 chars truncated\u{2026}");
    // A per-record override is in tokens and holds its allowance already.
    let i = passes(&build(
        bytes(10),
        x(13),
        Some(json!({"fallback_token_limit_override": 3})),
    ));
    witnessed_saying(&i, "xxxxxx\u{2026}1 tokens truncated\u{2026}xxxxxx");
    passes(&build(
        bytes(1),
        x(13),
        Some(json!({"fallback_token_limit_override": 100})),
    ));
    assert_eq!(
        r5_outcome(&build(
            bytes(10),
            x(13),
            Some(json!({"fallback_token_limit_override": "3"}))
        )),
        "refused@{response_item}.metadata.fallback_token_limit_override"
    );
    // A list the budget shortens: her window would hold a truncated part
    // or an omission notice, and how to keep its full parts beside it is
    // not modeled, so it fails (no real list output is shortened).
    let parts = |a: usize, b: usize| {
        json!([{"type": "input_text", "text": "x".repeat(a)}, {"type": "input_text", "text": ""},
            {"type": "input_text", "text": "y".repeat(b)}])
    };
    for policy in [bytes(10), Some(json!({"mode": "tokens", "limit": 2}))] {
        passes(&build(policy.clone(), parts(8, 4), None));
        assert_eq!(
            r5_outcome(&build(policy.clone(), parts(8, 5), None)),
            output
        );
        // Budget used up before an empty part: codex 0.78 to 0.132 count it
        // omitted.
        assert_eq!(
            r5_outcome(&build(policy.clone(), parts(12, 0), None)),
            output
        );
    }
    // An image part costs nothing; text after it is still budgeted.
    let image_then = |n: usize| json!([{"type": "input_image", "image_url": PNG}, {"type": "input_text", "text": "x".repeat(n)}]);
    let i = passes(&build(bytes(10), image_then(12), None));
    assert_eq!(
        r5_window(&i),
        "<header> , \"Sys.\" , <other> , \"Go.\" , \"Ok.\" , \"Again.\" , [exec]\"ls\" , \
         <other> , <blob 8B> , \"xxxxxxxxxxxx\""
    );
    assert_eq!(r5_outcome(&build(bytes(10), image_then(13), None)), output);
}

/// The reasoning summary parts, by their place.
fn summary_parts(imported: &Imported) -> Vec<(u128, String)> {
    use triblespace::prelude::inlineencodings::U256BE;
    let facts = imported.fragment.facts();
    let found: Vec<(Id, Inline<U256BE>, Inline<Handle<UTF8String>>)> = find!(
        (gloss: Id, part: Inline<U256BE>, text: Inline<Handle<UTF8String>>),
        pattern!(facts, [{ ?gloss @
            v3::gloss::category: v3::gloss::category::REASONING_SUMMARY,
            v3::gloss::part: ?part,
            v3::gloss::text: ?text,
        }])
    )
    .collect();
    let mut out: Vec<(u128, String)> = found
        .into_iter()
        .map(|(_, part, text)| {
            (
                part.try_from_inline::<u128>().unwrap(),
                text_of(imported, text),
            )
        })
        .collect();
    out.sort();
    out
}

/// X16 (review `r5l_cx_reasoning_summaries`): a reasoning summary's parts
/// keep their order and count, each a gloss with its place.
#[test]
fn r5l_cx_reasoning_summaries() {
    let build = |summary: Value, token: Value| {
        let mut r = Vec::new();
        rv_head(&mut r);
        r.push(rv_user(2, "Q."));
        r.push(line(
            3,
            "response_item",
            json!({"type": "reasoning", "summary": summary, "content": null,
                "encrypted_content": token}),
        ));
        r.push(rv_said(4, "A.", Some("final_answer")));
        jsonl(&r)
    };
    let st = |t: &str| json!({"type": "summary_text", "text": t});
    let r = json!(REASONING);
    let three = build(
        json!([st("**One**"), st("**Two**"), st("**Three**")]),
        r.clone(),
    );
    let i = passes(&three);
    assert_eq!(
        summary_parts(&i),
        vec![
            (0, "**One**".to_owned()),
            (1, "**Two**".to_owned()),
            (2, "**Three**".to_owned())
        ]
    );
    assert_eq!(blob_sizes(&i, v3::fact::modality::THINKING), vec![23]);
    let reordered = build(
        json!([st("**Two**"), st("**One**"), st("**Three**")]),
        r.clone(),
    );
    assert!(!gate(&reordered, &i).passed());
    assert!(!gate(&build(json!([st("**One**"), st("**Two**")]), r.clone()), &i).passed());
    assert!(r5_differ(&three, &reordered));
    assert!(r5_differ(
        &three,
        &build(
            json!([st("**One**\n\n**Two**"), st("**Three**")]),
            r.clone()
        )
    ));
    // A repeated part is a second gloss.
    let one = build(json!([st("**One**")]), r.clone());
    let twice = build(json!([st("**One**"), st("**One**")]), r.clone());
    assert!(r5_differ(&one, &twice));
    let i = passes(&twice);
    assert_eq!(
        summary_parts(&i),
        vec![(0, "**One**".to_owned()), (1, "**One**".to_owned())]
    );
    assert!(!gate(&one, &i).passed());
    assert!(!gate(&twice, &import_ok(&one)).passed());
    let none = build(json!([]), r.clone());
    let n = passes(&none);
    assert!(glosses(&n).is_empty());
    assert!(r5_differ(&none, &three));
    assert!(r5_differ(
        &none,
        &build(json!([]), json!("b3RoZXIgdG9rZW4="))
    ));
    // An empty part holds no gloss, and the parts after it keep their place.
    let i = passes(&build(json!([st(""), st("**Two**")]), r.clone()));
    assert_eq!(summary_parts(&i), vec![(1, "**Two**".to_owned())]);
    assert_eq!(
        r5_outcome(&build(json!(null), r.clone())),
        "imported gate=true"
    );
}

/// Her call arguments (review `r5l_cx_arguments_text`): accepted by JP's
/// rule, key order, whitespace and escape spelling are not kept; number
/// spellings are, and duplicate keys and trailing garbage fail.
#[test]
fn r5l_cx_arguments_text() {
    let build = |args: &str| {
        let mut r = Vec::new();
        rv_head(&mut r);
        r.push(rv_user(2, "Q."));
        r.push(line(
            3,
            "response_item",
            json!({"type": "function_call", "name": "shell", "arguments": args,
                "call_id": "c1"}),
        ));
        r.push(line(
            4,
            "response_item",
            json!({"type": "function_call_output", "call_id": "c1", "output": "ok"}),
        ));
        r.push(rv_said(5, "A.", Some("final_answer")));
        jsonl(&r)
    };
    let base = build(r#"{"command":["ls"],"timeout":1000}"#);
    let i = passes(&base);
    for args in [
        r#"{"timeout":1000,"command":["ls"]}"#,
        r#"{"command": ["ls"], "timeout": 1000}"#,
        "{\n  \"command\": [\"ls\"],\n  \"timeout\": 1000\n}",
    ] {
        let other = build(args);
        assert!(!r5_differ(&base, &other));
        assert!(gate(&other, &i).passed());
    }
    let other = build(r#"{"command":["ls"],"timeout":1e3}"#);
    assert!(r5_differ(&base, &other));
    assert!(!gate(&other, &i).passed());
    let arguments = "refused@{response_item}.payload{function_call}.arguments";
    assert_eq!(
        r5_outcome(&build(r#"{"command":["ls"],"command":["rm"]}"#)),
        arguments
    );
    assert_eq!(r5_outcome(&build(r#"{"command":["ls"]} x"#)), arguments);
}

/// Tool output lists (review `r5l_cx_output_parts`): parts keep their
/// order, image bytes and detail; empty text parts hold nothing.
#[test]
fn r5l_cx_output_parts() {
    let build = |parts: Value| {
        let mut r = Vec::new();
        rv_head(&mut r);
        r.push(rv_user(2, "Q."));
        r.push(line(
            3,
            "response_item",
            json!({"type": "custom_tool_call", "name": "view", "input": "a.png",
                "call_id": "v1"}),
        ));
        r.push(line(
            4,
            "response_item",
            json!({"type": "custom_tool_call_output", "call_id": "v1", "output": parts}),
        ));
        r.push(rv_said(5, "A.", Some("final_answer")));
        jsonl(&r)
    };
    let t = |s: &str| json!({"type": "input_text", "text": s});
    let img = json!({"type": "input_image", "image_url": PNG});
    let source = build(json!([t("Before."), t(""), img.clone(), t("After.")]));
    let i = passes(&source);
    let a = witnessed_saying(&i, "A.");
    let mut v = vec![];
    let mut cur = previous(&i, a);
    while let Some(n) = cur {
        v.push(r5_label(&i, n));
        cur = previous(&i, n);
    }
    v.reverse();
    assert_eq!(
        v.join(" , "),
        "<header> , \"Sys.\" , <other> , \"Q.\" , [view]\"a.png\" , <other> , \
         \"Before.\" , <blob 8B> , \"After.\""
    );
    assert!(!r5_differ(
        &source,
        &build(json!([t("Before."), img.clone(), t("After.")]))
    ));
    for other in [
        json!([t("After."), t(""), img.clone(), t("Before.")]),
        json!([img.clone(), t("Before."), t(""), t("After.")]),
        json!([t("Before."), t(""), {"type": "input_image",
            "image_url": "data:image/gif;base64,R0lGODlhAQABAAAAACw="}, t("After.")]),
    ] {
        assert!(r5_differ(&source, &build(other)));
    }
    let detail = |d: &str| json!({"type": "input_image", "image_url": PNG, "detail": d});
    assert!(r5_differ(
        &build(json!([t("Before."), img.clone()])),
        &build(json!([t("Before."), detail("high")]))
    ));
    assert!(r5_differ(
        &build(json!([t("Before."), detail("low")])),
        &build(json!([t("Before."), detail("high")]))
    ));
    assert_eq!(
        r5_outcome(&build(json!([t("Before."), detail("high")]))),
        "imported gate=true"
    );
    assert_eq!(
        r5_outcome(&build(
            json!([{"type": "input_image", "image_url": "https://example.invalid/a.png"}])
        )),
        "refused@{response_item}.payload{custom_tool_call_output}.output[]{input_image}.image_url"
    );
    assert_eq!(r5_outcome(&build(json!([t("")]))), "imported gate=true");
    assert_eq!(r5_outcome(&build(json!([]))), "imported gate=true");
}

// ---------------------------------------------------------------------------
// The round-6 losslessness review's fixtures, as permanent tests (round 6b).

/// Whether the gate passes `source` against the import of `other`.
fn r6_cross(source: &str, other: &str) -> bool {
    gate(source, &import_ok(other)).passed()
}

fn r6_window_before(imported: &Imported, text: &str) -> String {
    let done = witnessed_saying(imported, text);
    let mut v = vec![];
    let mut cur = previous(imported, done);
    while let Some(n) = cur {
        v.push(r5_label(imported, n));
        cur = previous(imported, n);
    }
    v.reverse();
    v.join(" , ")
}

/// The prefixes of `records` that import and add tribles the full import
/// does not hold; the full import must pass the gate.
fn r6_prefixes_adding(records: &[Value]) -> Vec<usize> {
    let full = jsonl(records);
    let live = passes(&full);
    (1..records.len())
        .filter(|&k| {
            import(jsonl(&records[..k]).as_bytes())
                .is_ok_and(|fragment| fragment.facts().difference(live.fragment.facts()).len() > 0)
        })
        .collect()
}

/// One tool output under a header that records no budget, written by a
/// client of `version` (`None`: a `session_meta` without `cli_version`).
fn x18p_source(version: Option<&str>, model: &str, output: Value) -> String {
    let mut meta = json!({"id": "77777777-7777-4777-8777-777777777777", "timestamp": stamp(0),
        "cwd": "/synthetic", "originator": "codex_cli_rs",
        "model_provider": "openai", "base_instructions": {"text": "Sys."}});
    if let Some(version) = version {
        meta["cli_version"] = json!(version);
    }
    jsonl(&[
        line(0, "session_meta", meta),
        line(
            1,
            "event_msg",
            json!({"type": "task_started", "turn_id": "t1"}),
        ),
        line(
            1,
            "turn_context",
            json!({"model": model, "effort": "high", "summary": "auto", "turn_id": "t1"}),
        ),
        rv_user(2, "Go."),
        line(
            3,
            "response_item",
            json!({"type": "custom_tool_call", "name": "exec", "input": "ls", "call_id": "o1"}),
        ),
        line(
            4,
            "response_item",
            json!({"type": "custom_tool_call_output", "call_id": "o1", "output": output}),
        ),
        rv_said(5, "Done.", Some("final_answer")),
    ])
}

/// X18': where no budget is recorded, the window she saw is unknown for an
/// output that any budget her client could have run under would cut. Codex
/// 0.51 to 0.58 cut at 10 KiB or 256 lines (`MODEL_FORMAT_MAX_BYTES`,
/// `MODEL_FORMAT_MAX_LINES`); 0.59 to 0.77 and 0.133 on cut under the
/// model's policy times 1.2, whose smallest documented value is
/// Bytes(10_000), so 12,000 bytes. Such an output fails the unit as Open,
/// at the importer and in the gate's walk, and never reaches her window
/// whole (r6l_cx_unrecorded_budget). Outputs within that bound, and clients
/// before 0.51 (which cut nothing when recording), are unaffected.
#[test]
fn r6l_cx_unrecorded_budget() {
    let path = "{response_item}.payload{custom_tool_call_output}.output";
    let open = |source: &str| {
        let failure = refused(source);
        assert_eq!(failure.path, path, "{}", failure.reason);
        assert!(failure.reason.contains("unrecorded"), "{}", failure.reason);
        assert!(
            walk_rows(source).contains(&(path.to_owned(), Kind::Open)),
            "the gate's walk lets the output through"
        );
    };
    let whole = |source: &str, n: usize| {
        let imported = passes(source);
        assert_eq!(nodes_saying(&imported, &"x".repeat(n)).len(), 1);
        assert!(untruncated(&imported).is_empty());
    };
    let x = |n: usize| json!("x".repeat(n));
    // The reviewer's four: each over its generation's smallest budget.
    for (version, model, n) in [
        ("0.140.0", "gpt-5.2", 13_000usize),
        ("0.140.0", "gpt-5.5", 50_000),
        ("0.70.0", "gpt-5", 13_000),
        ("0.55.0", "gpt-5", 11_000),
    ] {
        open(&x18p_source(Some(version), model, x(n)));
    }
    // At the bound: unaffected.
    whole(&x18p_source(Some("0.140.0"), "gpt-5.5", x(12_000)), 12_000);
    whole(&x18p_source(Some("0.70.0"), "gpt-5", x(12_000)), 12_000);
    whole(&x18p_source(Some("0.55.0"), "gpt-5", x(10_240)), 10_240);
    // 0.51 to 0.58 also cut by lines.
    let lines = "x\n".repeat(257);
    open(&x18p_source(Some("0.55.0"), "gpt-5", json!(lines)));
    let lines = "x\n".repeat(256);
    passes(&x18p_source(Some("0.55.0"), "gpt-5", json!(lines)));
    passes(&x18p_source(
        Some("0.140.0"),
        "gpt-5",
        json!("x\n".repeat(300)),
    ));
    // Before 0.51 the recorded output is what she saw.
    whole(&x18p_source(Some("0.50.0"), "gpt-5", x(50_000)), 50_000);
    // An unknown client: any generation's bound.
    open(&x18p_source(None, "gpt-5", x(11_000)));
    open(&x18p_source(Some("0.0.0"), "gpt-5", x(11_000)));
    open(&x18p_source(Some("not a version"), "gpt-5", x(11_000)));
    // A list output over the bound.
    let parts = json!([{"type": "input_text", "text": "x".repeat(13_000)}]);
    open(&x18p_source(Some("0.140.0"), "gpt-5.5", parts));
    let parts = json!([{"type": "input_text", "text": "x".repeat(100)}]);
    passes(&x18p_source(Some("0.140.0"), "gpt-5.5", parts));
    // A bare item predates the wrapped lines 0.50 writes: its client cut
    // nothing when recording.
    let long = "x".repeat(50_000);
    let bare = jsonl(&[
        json!({"id": "11111111-1111-4111-8111-111111111111", "timestamp": "2025-07-07T20:00:00.000Z"}),
        json!({"type": "message", "role": "user",
            "content": [{"type": "input_text", "text": "List the files."}]}),
        json!({"type": "function_call", "name": "shell", "arguments": "{}", "call_id": "call_1"}),
        json!({"type": "function_call_output", "call_id": "call_1", "output": long}),
        json!({"type": "message", "role": "assistant",
            "content": [{"type": "output_text", "text": "Done."}]}),
    ]);
    let imported = passes(&bare);
    assert_eq!(nodes_saying(&imported, &long).len(), 1);
}

/// X18: two full outputs with the same truncated window differ only in
/// the `untruncated` claim beside it, and the gate tells them apart.
#[test]
fn r6l_cx_untruncated_distinguishes() {
    let build = |output: &str| {
        let mut r = vec![rv_meta(0, "77777777-7777-4777-8777-777777777777", "Sys.")];
        r.push(line(
            1,
            "event_msg",
            json!({"type": "task_started", "turn_id": "t1"}),
        ));
        r.push(line(
            1,
            "turn_context",
            json!({"model": "gpt-5.5", "effort": "high", "summary": "auto", "turn_id": "t1",
                "truncation_policy": {"mode": "bytes", "limit": 10}}),
        ));
        r.push(rv_user(2, "Go."));
        r.push(line(
            3,
            "response_item",
            json!({"type": "function_call", "name": "shell", "arguments": "{}", "call_id": "o1"}),
        ));
        r.push(line(
            4,
            "response_item",
            json!({"type": "function_call_output", "call_id": "o1", "output": output}),
        ));
        r.push(rv_said(5, "Done.", Some("final_answer")));
        r
    };
    let a = jsonl(&build("xxxxxxAxxxxxx"));
    let b = jsonl(&build("xxxxxxBxxxxxx"));
    let ia = passes(&a);
    assert_eq!(
        r6_window_before(&ia, "Done."),
        "<header> , \"Sys.\" , <other> , \"Go.\" , [shell] , <other> , \
         \"xxxxxx\u{2026}1 chars truncated\u{2026}xxxxxx\""
    );
    assert_eq!(untruncated(&ia), vec!["xxxxxxAxxxxxx".to_owned()]);
    assert!(r5_differ(&a, &b));
    assert!(!r6_cross(&b, &a));
    assert!(r6_prefixes_adding(&build("xxxxxxAxxxxxx")).is_empty());
    let c = jsonl(&build("aaaaa\u{e9}\u{e9}bbbbb"));
    let ic = passes(&c);
    assert!(
        r6_window_before(&ic, "Done.").ends_with("\"aaaaa\u{2026}2 chars truncated\u{2026}bbbbb\"")
    );
}

/// X17 beyond the permanent test: in a rollout without `task_started`, an
/// abort after her reasoning or her call ends that item, and no prefix
/// adds anything.
#[test]
fn r6l_cx_x17_more() {
    let prompt = |s: u32, text: &str| {
        [
            rv_user(s, text),
            line(
                s,
                "event_msg",
                json!({"type": "user_message", "message": text, "images": []}),
            ),
        ]
    };
    let aborted = |s: u32| {
        line(
            s,
            "event_msg",
            json!({"type": "turn_aborted", "reason": "interrupted"}),
        )
    };
    let head = || {
        vec![
            rv_meta(0, "77777777-7777-4777-8777-777777777777", "Sys."),
            line(
                1,
                "turn_context",
                json!({"model": "gpt-5", "effort": "high", "summary": "auto"}),
            ),
        ]
    };
    let mut r = head();
    r.extend(prompt(2, "P1."));
    r.push(rv_said(3, "R1.", None));
    r.extend(prompt(4, "P2."));
    r.push(line(
        5,
        "response_item",
        json!({"type": "reasoning", "summary": [], "content": null, "encrypted_content": REASONING}),
    ));
    r.push(aborted(6));
    r.extend(prompt(7, "P3."));
    r.push(rv_said(8, "R3.", None));
    let i = passes(&jsonl(&r));
    assert_eq!(
        r5_claims(&i),
        "ends:interrupted@<blob 23B> turn_ends:interrupted@<blob 23B>"
    );
    assert!(r6_prefixes_adding(&r).is_empty());
    let mut r = head();
    r.extend(prompt(2, "P1."));
    r.push(rv_said(3, "R1.", None));
    r.extend(prompt(4, "P2."));
    r.push(line(
        5,
        "response_item",
        json!({"type": "function_call", "name": "shell", "arguments": "{}", "call_id": "c1"}),
    ));
    r.push(aborted(6));
    r.extend(prompt(7, "P3."));
    r.push(rv_said(8, "R3.", None));
    let i = passes(&jsonl(&r));
    assert_eq!(
        r5_claims(&i),
        "ends:interrupted@[shell] turn_ends:interrupted@[shell]"
    );
    let mut r = head();
    r.extend(prompt(2, "P1."));
    r.push(rv_said(3, "R1.", None));
    r.extend(prompt(4, "P2."));
    r.push(aborted(5));
    r.extend(prompt(6, "P3."));
    r.push(rv_said(7, "R3.", None));
    assert!(r6_prefixes_adding(&r).is_empty());
    // A prompt held only by its event copy fails.
    let mut r = head();
    r.extend(prompt(2, "P1."));
    r.push(rv_said(3, "R1.", None));
    r.push(line(
        4,
        "event_msg",
        json!({"type": "user_message", "message": "P2.", "images": []}),
    ));
    r.push(aborted(5));
    r.extend(prompt(6, "P3."));
    r.push(rv_said(7, "R3.", None));
    assert_eq!(
        r5_outcome(&jsonl(&r)),
        "refused@{event_msg}.payload{user_message}.message"
    );
}

/// X16 beyond the permanent test: the client's event copies of the parts,
/// two reasoning items with one summary, and an unnamed part type.
#[test]
fn r6l_cx_x16_more() {
    let st = |t: &str| json!({"type": "summary_text", "text": t});
    let reasoning = |s: u32, summary: Value| {
        line(
            s,
            "response_item",
            json!({"type": "reasoning", "summary": summary, "content": null, "encrypted_content": REASONING}),
        )
    };
    let ev = |s: u32, t: &str| {
        line(
            s,
            "event_msg",
            json!({"type": "agent_reasoning", "text": t}),
        )
    };
    let build = |events: &[&str], summary: Value| {
        let mut r = Vec::new();
        rv_head(&mut r);
        r.push(rv_user(2, "Q."));
        for e in events {
            r.push(ev(3, e));
        }
        r.push(reasoning(3, summary));
        r.push(rv_said(4, "A.", Some("final_answer")));
        jsonl(&r)
    };
    let two = json!([st("**One**"), st("**Two**")]);
    for events in [
        &["**One**", "**Two**"],
        &["**Two**", "**One**"],
        &["**One**", "**One**"],
    ] {
        assert_eq!(
            r5_outcome(&build(events, two.clone())),
            "imported gate=true"
        );
    }
    assert_eq!(
        r5_outcome(&build(&["**Three**"], two.clone())),
        "refused@{event_msg}.payload{agent_reasoning}.text"
    );
    assert!(!r5_differ(
        &build(&[], two.clone()),
        &build(&["**One**", "**Two**"], two.clone())
    ));
    assert_eq!(
        r5_outcome(&build(
            &[],
            json!([st("**One**"), {"type": "summary_image", "text": "x"}])
        )),
        "refused@{response_item}.payload{reasoning}.summary[]{summary_image}.text"
    );
    assert_eq!(
        r5_outcome(&build(&[], json!([{"type": "summary_text", "text": 7}]))),
        "refused@{response_item}.payload{reasoning}.summary[]{summary_text}.text"
    );
    let mut r = Vec::new();
    rv_head(&mut r);
    r.push(rv_user(2, "Q."));
    r.push(reasoning(3, json!([st("**Same**")])));
    r.push(line(
        3,
        "response_item",
        json!({"type": "function_call", "name": "shell", "arguments": "{}", "call_id": "c1"}),
    ));
    r.push(line(
        3,
        "response_item",
        json!({"type": "function_call_output", "call_id": "c1", "output": "ok"}),
    ));
    r.push(reasoning(3, json!([st("**Same**")])));
    r.push(rv_said(4, "A.", Some("final_answer")));
    let s = jsonl(&r);
    let i = passes(&s);
    assert_eq!(
        summary_parts(&i),
        vec![(0, "**Same**".to_owned()), (0, "**Same**".to_owned())]
    );
    let mut r1 = r.clone();
    let last = r1.len() - 2;
    r1[last] = reasoning(3, json!([]));
    assert!(r5_differ(&s, &jsonl(&r1)));
    assert!(!r6_cross(&jsonl(&r1), &s));
}

/// His message parts: order and count are kept. An empty `input_text`
/// part (C8') fails as Open: codex forwards the part to the server, and
/// what her window held for it is unknown.
#[test]
fn r6l_cx_user_parts() {
    let build = |parts: &[&str]| {
        let mut r = Vec::new();
        rv_head(&mut r);
        let content: Vec<Value> = parts
            .iter()
            .map(|t| json!({"type": "input_text", "text": t}))
            .collect();
        r.push(line(
            2,
            "response_item",
            json!({"type": "message", "role": "user", "content": content}),
        ));
        r.push(rv_said(3, "A.", Some("final_answer")));
        jsonl(&r)
    };
    let abc = build(&["<env>x</env>", "<inst>y</inst>", "Prompt."]);
    let i = passes(&abc);
    assert_eq!(
        r6_window_before(&i, "A."),
        "<header> , \"Sys.\" , <other> , \"<env>x</env>\" , \"<inst>y</inst>\" , \"Prompt.\""
    );
    let swapped = build(&["<inst>y</inst>", "<env>x</env>", "Prompt."]);
    assert!(r5_differ(&abc, &swapped));
    assert!(!r6_cross(&swapped, &abc));
    let twice = build(&["Prompt.", "Prompt."]);
    let once = build(&["Prompt."]);
    assert!(r5_differ(&once, &twice));
    assert!(!r6_cross(&once, &twice));
    let joined = build(&["<env>x</env><inst>y</inst>", "Prompt."]);
    assert!(r5_differ(&abc, &joined));
    assert!(!r6_cross(&joined, &abc));
    let path = "{response_item}.payload{message}{user}.content[]{input_text}.text";
    let empty = build(&["", "Prompt."]);
    assert_eq!(refused(&empty).path, path);
    assert!(walk_rows(&empty).contains(&(path.to_owned(), Kind::Open)));
}

/// Exact text of her freeform call inputs and of outputs, and outputs that
/// hold a JSON document in a string, kept as written.
#[test]
fn r6l_cx_text_exact() {
    let build = |input: &str, output: &str| {
        let mut r = Vec::new();
        rv_head(&mut r);
        r.push(rv_user(2, "Q."));
        r.push(line(
            3,
            "response_item",
            json!({"type": "custom_tool_call", "name": "apply_patch", "input": input, "call_id": "p1"}),
        ));
        r.push(line(
            4,
            "response_item",
            json!({"type": "custom_tool_call_output", "call_id": "p1", "output": output}),
        ));
        r.push(rv_said(5, "A.", Some("final_answer")));
        jsonl(&r)
    };
    let base = build("*** Begin Patch\n*** End Patch\n", "ok");
    passes(&base);
    assert!(r5_differ(
        &base,
        &build("*** Begin Patch\n*** End Patch", "ok")
    ));
    assert!(r5_differ(
        &base,
        &build("*** Begin Patch\r\n*** End Patch\r\n", "ok")
    ));
    assert!(r5_differ(&build("p", "a\r\nb"), &build("p", "a\nb")));
    assert!(r5_differ(
        &build("p", "caf\u{e9}"),
        &build("p", "cafe\u{301}")
    ));
    let j1 = build(
        "p",
        r#"{"output":"x","metadata":{"exit_code":0,"duration_seconds":0.1}}"#,
    );
    let j2 = build(
        "p",
        r#"{"metadata":{"exit_code":0,"duration_seconds":0.1},"output":"x"}"#,
    );
    let j3 = build(
        "p",
        r#"{"output": "x", "metadata": {"exit_code": 0, "duration_seconds": 0.1}}"#,
    );
    passes(&j1);
    for other in [&j2, &j3] {
        assert!(r5_differ(&j1, other));
        assert!(!r6_cross(other, &j1));
    }
}

/// The `namespace` of her function calls is kept.
#[test]
fn r6l_cx_call_namespace() {
    let build = |ns: Option<&str>| {
        let mut r = Vec::new();
        rv_head(&mut r);
        r.push(rv_user(2, "Q."));
        let mut call =
            json!({"type": "function_call", "name": "read", "arguments": "{}", "call_id": "c1"});
        if let Some(ns) = ns {
            call["namespace"] = json!(ns);
        }
        r.push(line(3, "response_item", call));
        r.push(line(
            4,
            "response_item",
            json!({"type": "function_call_output", "call_id": "c1", "output": "ok"}),
        ));
        r.push(rv_said(5, "A.", Some("final_answer")));
        jsonl(&r)
    };
    let none = build(None);
    let a = build(Some("mcp__a"));
    let b = build(Some("mcp__b"));
    assert_eq!(r5_outcome(&none), "imported gate=true");
    assert_eq!(r5_outcome(&a), "imported gate=true");
    assert!(r5_differ(&none, &a));
    assert!(r5_differ(&a, &b));
    assert!(!r6_cross(&b, &a));
}
