//! Hand-stated fixtures for the relational half, Claude Code first. Each
//! states one relational decision and its expected window by hand: the
//! modalities and gap kinds along it, root first. None is computed from the
//! walker or the importer.

use serde_json::{json, Value};
use triblespace::core::blob::MemoryBlobStore;
use triblespace::core::import::json_tree::JsonTreeImporter;
use triblespace::core::repo::BlobStore;

use super::super::{compile, lines};
use super::*;
use crate::archive_v3::fields::claude_code::TABLE;
use crate::schemas::archive_v3 as v3;

use v3::fact::absent::{BODY, SPAN};
use v3::fact::modality::{SYSTEM, TEXT, TOOL_DEFINITION};

/// The modality a span gap of unknown kind has in a window: none.
const UNKNOWN: Id = triblespace::macros::id_hex!("AD62B67CF8B7E4C7A0A6DA03F6A62A08");

/// What a Claude Code window opens with before anything is recorded: the
/// settings header gap, the system prompt gap, the tool definitions span.
const SECTION: [(Id, Option<Id>); 3] = [
    (SYSTEM, Some(BODY)),
    (SYSTEM, Some(BODY)),
    (TOOL_DEFINITION, Some(SPAN)),
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

fn user(uuid: &str, parent: Option<&str>, second: u32, text: &str) -> Value {
    json!({
        "type": "user", "sessionId": "s-1", "uuid": uuid, "parentUuid": parent,
        "timestamp": stamp(second), "cwd": "/synthetic", "version": "0.0.0",
        "isSidechain": false, "userType": "external",
        "message": {"role": "user", "content": text}
    })
}

fn assistant(uuid: &str, parent: &str, second: u32, text: &str) -> Value {
    json!({
        "type": "assistant", "sessionId": "s-1", "uuid": uuid, "parentUuid": parent,
        "timestamp": stamp(second), "requestId": "req-m1",
        "message": {"id": "m1", "type": "message", "role": "assistant",
            "model": "claude-opus-4-6", "content": [{"type": "text", "text": text}],
            "stop_reason": "end_turn", "stop_sequence": null, "usage": {"input_tokens": 1}}
    })
}

/// The unit's facts and relations together, or why the relations refused.
fn built(source: &str) -> Result<Fragment, String> {
    let records: Vec<&str> = source.lines().filter(|l| !l.trim().is_empty()).collect();
    let array = format!("[{}]", records.join(","));
    let mut store = MemoryBlobStore::new();
    let unit = JsonTreeImporter::<_>::new(&mut store, None)
        .import_str(&array)
        .expect("synthetic JSON");
    let root = unit.root().expect("rooted");
    let blobs = unit.blobs().clone().snapshot().unwrap();
    let grammar = compile(&TABLE).expect("the table compiles");
    let image = grammar
        .cover(Unit::new(unit.facts(), &blobs), root, &lines())
        .map_err(|findings| format!("{findings:?}"))?;
    let relations = build(&TABLE, Unit::new(unit.facts(), &blobs), root, &image)?;
    Ok(image + relations)
}

/// The node holding the text fact `text`.
fn node_saying(fragment: &Fragment, text: &str) -> Id {
    let handle = {
        let blob: Blob<blobencodings::UTF8String> = text.to_blob();
        blob.get_handle()
    };
    let facts = fragment.facts();
    let found: Vec<Id> = find!(
        node: Id,
        pattern!(facts, [
            { ?node @ v3::node::holds: _?fact },
            { _?fact @ v3::fact::payload: handle },
        ])
    )
    .collect();
    assert_eq!(found.len(), 1, "one node holds {text:?}");
    found[0]
}

/// The modalities and gap kinds along the window before `node`, root first.
fn window_before(fragment: &Fragment, node: Id) -> Vec<(Id, Option<Id>)> {
    let facts = fragment.facts();
    let previous = |at: Id| -> Option<Id> {
        find!(p: Id, pattern!(facts, [{ at @ v3::node::previous: ?p }])).next()
    };
    let mut out = Vec::new();
    let mut cursor = previous(node);
    while let Some(at) = cursor {
        let modality = find!(
            m: Id,
            pattern!(facts, [{ at @ v3::node::holds: _?f }, { _?f @ v3::fact::modality: ?m }])
        )
        .next();
        let absent = find!(
            a: Id,
            pattern!(facts, [{ at @ v3::node::holds: _?f }, { _?f @ v3::fact::absent: ?a }])
        )
        .next();
        out.push((modality.unwrap_or(UNKNOWN), absent));
        cursor = previous(at);
    }
    out.reverse();
    out
}

fn section_then(items: &[(Id, Option<Id>)]) -> Vec<(Id, Option<Id>)> {
    SECTION
        .iter()
        .copied()
        .chain(items.iter().copied())
        .collect()
}

#[test]
fn a_reply_follows_its_prompt_after_the_unrecorded_section() {
    let source = jsonl(&[
        user("u1", None, 0, "Hi."),
        assistant("a1", "u1", 1, "Hello."),
    ]);
    let built = built(&source).expect("builds");
    let hello = node_saying(&built, "Hello.");
    assert_eq!(window_before(&built, hello), section_then(&[(TEXT, None)]));
}

#[test]
fn a_record_whose_parent_comes_later_in_the_file_is_refused() {
    let source = jsonl(&[
        assistant("a1", "u1", 1, "Hello."),
        user("u1", None, 0, "Hi."),
    ]);
    let refused = built(&source).expect_err("refused");
    assert!(refused.contains("{user}.parentUuid"), "{refused}");
}

#[test]
fn a_repeated_record_is_one_record_and_a_changed_repeat_is_refused() {
    let once = jsonl(&[
        user("u1", None, 0, "Hi."),
        assistant("a1", "u1", 1, "Hello."),
    ]);
    let twice = jsonl(&[
        user("u1", None, 0, "Hi."),
        user("u1", None, 0, "Hi."),
        assistant("a1", "u1", 1, "Hello."),
    ]);
    let once = built(&once).expect("builds");
    let twice = built(&twice).expect("a faithful repeat builds");
    assert_eq!(once.facts(), twice.facts(), "the repeat adds nothing");

    let changed = jsonl(&[
        user("u1", None, 0, "Hi."),
        user("u1", None, 0, "Hi there."),
        assistant("a1", "u1", 1, "Hello."),
    ]);
    let refused = built(&changed).expect_err("refused");
    assert!(refused.contains("{user}.uuid"), "{refused}");
}

#[test]
fn a_missing_parent_opens_with_a_span_of_unknown_items() {
    let source = jsonl(&[
        user("u2", Some("gone"), 0, "Hi."),
        assistant("a1", "u2", 1, "Hello."),
    ]);
    let built = built(&source).expect("builds");
    let hello = node_saying(&built, "Hello.");
    assert_eq!(
        window_before(&built, hello),
        section_then(&[(UNKNOWN, Some(SPAN)), (TEXT, None)])
    );
}
