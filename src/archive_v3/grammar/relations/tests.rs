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
    // The refusal names the refusing record's own kind.
    assert!(refused.contains("{assistant}.parentUuid"), "{refused}");
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

#[test]
fn the_blocks_of_one_record_follow_each_other_in_array_order() {
    let mut reply = assistant("a1", "u1", 1, "One.");
    reply["message"]["content"] = json!([
        {"type": "text", "text": "One."},
        {"type": "text", "text": "Two."},
    ]);
    let source = jsonl(&[user("u1", None, 0, "Hi."), reply]);
    let built = built(&source).expect("builds");
    let two = node_saying(&built, "Two.");
    assert_eq!(
        window_before(&built, two),
        section_then(&[(TEXT, None), (TEXT, None)])
    );
    let one = node_saying(&built, "One.");
    assert_eq!(window_before(&built, one), section_then(&[(TEXT, None)]));
}

/// What a node holds, as (modality, gap kind): vocabulary, never values.
fn held(fragment: &Fragment, node: Id) -> (Id, Option<Id>) {
    let facts = fragment.facts();
    let modality = find!(
        m: Id,
        pattern!(facts, [{ node @ v3::node::holds: _?f }, { _?f @ v3::fact::modality: ?m }])
    )
    .next();
    let absent = find!(
        a: Id,
        pattern!(facts, [{ node @ v3::node::holds: _?f }, { _?f @ v3::fact::absent: ?a }])
    )
    .next();
    (modality.unwrap_or(UNKNOWN), absent)
}

fn name_of(term: (Id, Option<Id>)) -> String {
    let m = v3::fact::modality::SPECS
        .iter()
        .find(|(id, _)| *id == term.0)
        .map_or("?", |(_, n)| *n);
    let a = term
        .1
        .and_then(|a| v3::fact::absent::SPECS.iter().find(|(id, _)| *id == a))
        .map_or("", |(_, n)| *n);
    format!("{m}/{a}")
}

/// The first divergence: the importer's next item after the deepest node
/// both share on a path from the root, and ours, as vocabulary.
fn first_divergence(ours: &Fragment, theirs: &Fragment) -> Option<(String, String)> {
    let shared = nodes_of(ours);
    let tf = theirs.facts();
    let of = ours.facts();
    let mut best: Option<(usize, Id, Id)> = None;
    for (child, parent) in find!(
        (c: Id, p: Id),
        pattern!(tf, [{ ?c @ v3::node::previous: ?p }])
    ) {
        if shared.contains(&parent) && !shared.contains(&child) {
            let depth = window_before(theirs, child).len();
            if best.is_none_or(|(d, _, _)| depth < d) {
                best = Some((depth, child, parent));
            }
        }
    }
    let (_, child, parent) = best?;
    let next_ours: Vec<String> = find!(c: Id, pattern!(of, [{ ?c @ v3::node::previous: parent }]))
        .map(|c| name_of(held(ours, c)))
        .collect();
    Some((name_of(held(theirs, child)), next_ours.join("|")))
}

/// The node ids of a fragment.
fn nodes_of(fragment: &Fragment) -> std::collections::BTreeSet<Id> {
    let facts = fragment.facts();
    find!(n: Id, pattern!(facts, [{ ?n @ v3::node::holds: _?f }])).collect()
}

#[test]
#[ignore = "reads real transcripts named by AV3_SAMPLE; prints no values"]
fn real_sample_relations() {
    let list = std::env::var("AV3_SAMPLE").expect("AV3_SAMPLE names a file list");
    let files: Vec<String> = std::fs::read_to_string(list)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    let mut reasons: std::collections::BTreeMap<String, usize> = Default::default();
    let (mut built_files, mut same, mut ours_only, mut theirs_only) = (0, 0, 0, 0);
    let mut divergences: std::collections::BTreeMap<(String, String), usize> = Default::default();
    let mut unstable = 0;
    // Our extra nodes: what each holds, after what its parent holds.
    let mut extra: std::collections::BTreeMap<(String, String), usize> = Default::default();
    for file in &files {
        let Ok(bytes) = std::fs::read(file) else {
            continue;
        };
        let Ok(imported) = crate::archive_v3::claude_code::import(&bytes) else {
            continue;
        };
        let Ok(source) = std::str::from_utf8(&bytes) else {
            continue;
        };
        match built(source) {
            // A refusal reason is a table path or shape, never a value.
            Err(reason) => {
                let reason = reason
                    .split(':')
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(120)
                    .collect();
                *reasons.entry(reason).or_default() += 1;
            }
            Ok(fragment) => {
                built_files += 1;
                // The same unit must build the same relations every time.
                if built(source).map(|again| again.facts() == fragment.facts()) != Ok(true) {
                    unstable += 1;
                }
                let ours = nodes_of(&fragment);
                let theirs = nodes_of(&imported);
                same += ours.intersection(&theirs).count();
                ours_only += ours.difference(&theirs).count();
                let of = fragment.facts();
                for &n in ours.difference(&theirs) {
                    let parent = find!(p: Id, pattern!(of, [{ n @ v3::node::previous: ?p }]))
                        .next()
                        .map_or("root".to_owned(), |p| name_of(held(&fragment, p)));
                    let at = if theirs.contains(&n) { "" } else { "" };
                    let _ = at;
                    *extra
                        .entry((parent, name_of(held(&fragment, n))))
                        .or_default() += 1;
                }
                theirs_only += theirs.difference(&ours).count();
                if let Some(d) = first_divergence(&fragment, &imported) {
                    *divergences.entry(d).or_default() += 1;
                }
            }
        }
    }
    println!(
        "files built {built_files} (built differently twice: {unstable}); nodes: shared {same}, ours only {ours_only}, importer only {theirs_only}"
    );
    println!("-- first divergence: files, importer's next item, ours");
    for ((theirs, ours), n) in &divergences {
        println!("{n}\t{theirs}\t{ours}");
    }
    println!("-- our extra nodes: count, parent holds, node holds");
    for ((parent, node), n) in &extra {
        println!("{n}\t{parent}\t{node}");
    }
    println!("-- refused: files, reason");
    for (reason, n) in &reasons {
        println!("{n}\t{reason}");
    }
}
