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
    built_with(&TABLE, source)
}

/// [`built`], under another format's table.
fn built_with(table: &'static FieldTable, source: &str) -> Result<Fragment, String> {
    let records: Vec<&str> = source.lines().filter(|l| !l.trim().is_empty()).collect();
    let array = format!("[{}]", records.join(","));
    let mut store = MemoryBlobStore::new();
    let unit = JsonTreeImporter::<_>::new(&mut store, None)
        .import_str(&array)
        .expect("synthetic JSON");
    let root = unit.root().expect("rooted");
    let blobs = unit.blobs().clone().snapshot().unwrap();
    let grammar = compile(table).expect("the table compiles");
    let image = grammar
        .cover(Unit::new(unit.facts(), &blobs), root, &lines())
        .map_err(|findings| format!("{findings:?}"))?;
    let relations = build(table, Unit::new(unit.facts(), &blobs), root, &image)?;
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

/// Records are built parents first, so a parent written after its child
/// is still its parent; only parent links that form a cycle are refused.
/// (This fixture first said the opposite: I had read one refusal in the
/// importer's line lookup and missed that it orders records parents first,
/// claude_code.rs ~163.)
#[test]
fn a_record_whose_parent_comes_later_in_the_file_still_follows_it() {
    let source = jsonl(&[
        assistant("a1", "u1", 1, "Hello."),
        user("u1", None, 0, "Hi."),
    ]);
    let fragment = built(&source).expect("builds");
    let hello = node_saying(&fragment, "Hello.");
    assert_eq!(
        window_before(&fragment, hello),
        section_then(&[(TEXT, None)])
    );

    let cycle = jsonl(&[
        assistant("a1", "u1", 1, "Hello."),
        user("u1", Some("a1"), 0, "Hi."),
    ]);
    let refused = built(&cycle).expect_err("a cycle is refused");
    assert!(refused.contains("cycle"), "{refused}");
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

/// The occurrence ids of a fragment, and its (from, to) links.
fn occurrence_ids(fragment: &Fragment) -> std::collections::BTreeSet<Id> {
    let facts = fragment.facts();
    find!(o: Id, pattern!(facts, [{ ?o @ v3::occurrence::of: _?w }])).collect()
}

/// Annotations outside the identity cores, as (entity, value) pairs: the
/// occurrences' ends and turn ends, the nodes' renderings, and the glosses
/// (whose ids include the node they describe).
fn annotations_of(fragment: &Fragment) -> std::collections::BTreeSet<(&'static str, Id, Id)> {
    let facts = fragment.facts();
    let mut out = std::collections::BTreeSet::new();
    out.extend(
        find!((o: Id, e: Id), pattern!(facts, [{ ?o @ v3::witness::ends: ?e }]))
            .map(|(a, b)| ("ends", a, b)),
    );
    out.extend(
        find!((o: Id, e: Id), pattern!(facts, [{ ?o @ v3::witness::turn_ends: ?e }]))
            .map(|(a, b)| ("turn_ends", a, b)),
    );
    out.extend(
        find!((n: Id, r: Id), pattern!(facts, [{ ?n @ v3::node::rendering: ?r }]))
            .map(|(a, b)| ("rendering", a, b)),
    );
    out.extend(
        find!((g: Id, n: Id), pattern!(facts, [{ ?g @ v3::gloss::describes: ?n }]))
            .map(|(a, b)| ("gloss", a, b)),
    );
    out.extend(
        find!((n: Id, o: Id), pattern!(facts, [{ ?n @ v3::node::recalls: ?o }]))
            .map(|(a, b)| ("recalls", a, b)),
    );
    out.extend(
        find!((n: Id, t: Id), pattern!(facts, [{ ?n @ v3::node::continues: ?t }]))
            .map(|(a, b)| ("continues", a, b)),
    );
    out
}

fn links_of(fragment: &Fragment) -> std::collections::BTreeSet<(Id, Id)> {
    let facts = fragment.facts();
    find!(
        (from: Id, to: Id),
        pattern!(facts, [{ ?from @ v3::node::responds_to: ?to }])
    )
    .collect()
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
    // The format: its table and its importer (AV3_FORMAT, Claude Code by
    // default).
    let codex = std::env::var("AV3_FORMAT").as_deref() == Ok("codex");
    let table: &'static FieldTable = if codex {
        &crate::archive_v3::fields::codex::TABLE
    } else {
        &TABLE
    };
    let import = |bytes: &[u8]| {
        if codex {
            crate::archive_v3::codex::import(bytes)
        } else {
            crate::archive_v3::claude_code::import(bytes)
        }
    };
    // Files the importer refuses that the relations build, by the
    // importer's failure (a table path and a fixed message, never a value).
    let mut beyond: std::collections::BTreeMap<String, usize> = Default::default();
    // Occurrences and links, each as (shared, ours only, importer only).
    let (mut occurrences, mut links, mut annotations) = ((0, 0, 0), (0, 0, 0), (0, 0, 0));
    let mut by_kind: std::collections::BTreeMap<(&str, &str), usize> = Default::default();
    // Our extra nodes: what each holds, after what its parent holds.
    let mut extra: std::collections::BTreeMap<(String, String), usize> = Default::default();
    for file in &files {
        let Ok(bytes) = std::fs::read(file) else {
            continue;
        };
        let Ok(source) = std::str::from_utf8(&bytes) else {
            continue;
        };
        let imported = match import(&bytes) {
            Ok(imported) => imported,
            Err(failure) => {
                if built_with(table, source).is_ok() {
                    *beyond.entry(format!("{failure:?}")).or_default() += 1;
                }
                continue;
            }
        };
        match built_with(table, source) {
            // A refusal reason is a table path or shape, never a value.
            Err(reason) => {
                // "not built yet: <attribute or shape at path placed Place>"
                // is vocabulary whole; any other reason, up to its first colon.
                let reason: String = if reason.starts_with("not built yet") {
                    reason.chars().take(160).collect()
                } else {
                    reason
                        .split(':')
                        .next()
                        .unwrap_or("")
                        .chars()
                        .take(120)
                        .collect()
                };
                *reasons.entry(reason).or_default() += 1;
            }
            Ok(fragment) => {
                built_files += 1;
                // The same unit must build the same relations every time.
                if built_with(table, source).map(|again| again.facts() == fragment.facts())
                    != Ok(true)
                {
                    unstable += 1;
                }
                let ours = nodes_of(&fragment);
                let theirs = nodes_of(&imported);
                same += ours.intersection(&theirs).count();
                ours_only += ours.difference(&theirs).count();
                for (count, (a, b)) in [(
                    &mut occurrences,
                    (occurrence_ids(&fragment), occurrence_ids(&imported)),
                )] {
                    count.0 += a.intersection(&b).count();
                    count.1 += a.difference(&b).count();
                    count.2 += b.difference(&a).count();
                }
                let (a, b) = (annotations_of(&fragment), annotations_of(&imported));
                annotations.0 += a.intersection(&b).count();
                annotations.1 += a.difference(&b).count();
                annotations.2 += b.difference(&a).count();
                for (kind, _, _) in a.difference(&b) {
                    *by_kind.entry(("ours", *kind)).or_default() += 1;
                }
                for (kind, _, _) in b.difference(&a) {
                    *by_kind.entry(("importer", *kind)).or_default() += 1;
                }
                let (a, b) = (links_of(&fragment), links_of(&imported));
                links.0 += a.intersection(&b).count();
                links.1 += a.difference(&b).count();
                links.2 += b.difference(&a).count();
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
        "files built {built_files} (built differently twice: {unstable}; built where the importer refuses: {}); nodes: shared {same}, ours only {ours_only}, importer only {theirs_only}",
        beyond.values().sum::<usize>()
    );
    println!(
        "occurrences: shared {}, ours only {}, importer only {}; links: shared {}, ours only {}, importer only {}",
        occurrences.0, occurrences.1, occurrences.2, links.0, links.1, links.2
    );
    println!(
        "annotations (ends, turn ends, renderings, glosses, recalls, continues): shared {}, ours only {}, importer only {}",
        annotations.0, annotations.1, annotations.2
    );
    println!("-- annotations on one side only: count, side, kind");
    for ((side, kind), n) in &by_kind {
        println!("{n}\t{side}\t{kind}");
    }
    println!("-- first divergence: files, importer's next item, ours");
    for ((theirs, ours), n) in &divergences {
        println!("{n}\t{theirs}\t{ours}");
    }
    println!("-- our extra nodes: count, parent holds, node holds");
    for ((parent, node), n) in &extra {
        println!("{n}\t{parent}\t{node}");
    }
    println!("-- built where the importer refuses: files, its failure");
    for (failure, n) in &beyond {
        println!("{n}\t{failure}");
    }
    println!("-- refused: files, reason");
    for (reason, n) in &reasons {
        println!("{n}\t{reason}");
    }
}

/// The occurrences of `node`'s witnesses: each as (timestamps,
/// generated_by, ends), sorted.
fn occurrences_of(fragment: &Fragment, node: Id) -> Vec<(Vec<[u8; 32]>, Vec<Id>, Vec<Id>)> {
    let facts = fragment.facts();
    let mut out: Vec<(Vec<[u8; 32]>, Vec<Id>, Vec<Id>)> = find!(
        occurrence: Id,
        pattern!(facts, [
            { _?witness @ v3::witness::projects_to: node },
            { ?occurrence @ v3::occurrence::of: _?witness },
        ])
    )
    .map(|o| {
        let mut times: Vec<[u8; 32]> = find!(
            t: Inline<inlineencodings::NsTAIInterval>,
            pattern!(facts, [{ o @ v3::witness::timestamp: ?t }])
        )
        .map(|t| t.raw)
        .collect();
        times.sort();
        let mut models: Vec<Id> =
            find!(m: Id, pattern!(facts, [{ o @ v3::witness::generated_by: ?m }])).collect();
        models.sort();
        let mut ends: Vec<Id> =
            find!(e: Id, pattern!(facts, [{ o @ v3::witness::ends: ?e }])).collect();
        ends.sort();
        (times, models, ends)
    })
    .collect();
    out.sort();
    out
}

#[test]
fn a_reply_is_witnessed_with_its_record_s_claims_and_its_end() {
    let source = jsonl(&[
        user("u1", None, 0, "Hi."),
        assistant("a1", "u1", 1, "Hello."),
    ]);
    let built = built(&source).expect("builds");
    let hello = node_saying(&built, "Hello.");
    let at = crate::archive_v3::claude_code::time(&stamp(1))
        .expect("a time")
        .raw;
    let model = crate::archive_v3::Model::vendor("claude-opus-4-6")
        .expect("a model")
        .id;
    assert_eq!(
        occurrences_of(&built, hello),
        vec![(vec![at], vec![model], vec![v3::node::ends::END_TURN])]
    );
    // Her prompt's record claims its time; no model sampled it, and it ends
    // nothing.
    let hi = node_saying(&built, "Hi.");
    let at = crate::archive_v3::claude_code::time(&stamp(0))
        .expect("a time")
        .raw;
    assert_eq!(occurrences_of(&built, hi), vec![(vec![at], vec![], vec![])]);
}

#[test]
fn a_result_responds_to_the_call_it_names_on_its_own_line() {
    let mut call = assistant("a1", "u1", 1, "unused");
    call["message"]["content"] = json!([{"type": "tool_use", "id": "t1", "name": "Read",
        "input": {"file_path": "/x"}, "caller": {"type": "direct"}}]);
    call["message"]["stop_reason"] = json!("tool_use");
    let mut result = user("u2", Some("a1"), 2, "unused");
    result["message"]["content"] = json!([{"type": "tool_result", "tool_use_id": "t1",
        "content": "fine", "is_error": false}]);
    let source = jsonl(&[user("u1", None, 0, "Hi."), call, result]);
    let built = built(&source).expect("builds");
    let facts = built.facts();
    let responds: Vec<(Id, Id)> = find!(
        (from: Id, to: Id),
        pattern!(facts, [{ ?from @ v3::node::responds_to: ?to }])
    )
    .collect();
    assert_eq!(responds.len(), 1, "one result, one link");
    let (from, to) = responds[0];
    // The link runs from the result's head to the node holding the call.
    assert_eq!(held_modality(&built, from), v3::fact::modality::TOOL_RESULT);
    assert_eq!(held_modality(&built, to), v3::fact::modality::TOOL_CALL);
}

fn held_modality(fragment: &Fragment, node: Id) -> Id {
    let facts = fragment.facts();
    find!(
        m: Id,
        pattern!(facts, [{ node @ v3::node::holds: _?f }, { _?f @ v3::fact::modality: ?m }])
    )
    .next()
    .expect("the node holds a fact")
}

#[test]
fn a_request_s_effort_heads_its_window_and_the_history_is_re_presented() {
    let mut reply = assistant("a1", "u1", 1, "Hello.");
    reply["effort"] = json!("high");
    let source = jsonl(&[user("u1", None, 0, "Hi."), reply]);
    let built = built(&source).expect("builds");
    let facts = built.facts();
    let hello = node_saying_in(&built, "Hello.", 1);
    // The same shape as the unrecorded section, but headed by the effort.
    assert_eq!(window_before(&built, hello), section_then(&[(TEXT, None)]));
    let mut chain = Vec::new();
    let mut cursor = Some(hello);
    while let Some(at) = cursor {
        chain.push(at);
        cursor = find!(p: Id, pattern!(facts, [{ at @ v3::node::previous: ?p }])).next();
    }
    let root = *chain.last().unwrap();
    let effort: Vec<Id> = find!(
        e: Id,
        pattern!(facts, [{ root @ v3::node::holds: _?f }, { _?f @ v3::fact::effort: ?e }])
    )
    .collect();
    assert_eq!(
        effort,
        vec![v3::fact::effort::HIGH],
        "the request's effort heads it"
    );
    // Her prompt stands twice: under the header gap where her record holds
    // it, and re-presented under the effort, recalling the original.
    let prompts = nodes_saying(&built, "Hi.");
    assert_eq!(prompts.len(), 2, "the original and its copy");
    let copy = chain[1];
    assert!(prompts.contains(&copy));
    let recalled: Vec<Id> =
        find!(r: Id, pattern!(facts, [{ copy @ v3::node::recalls: ?r }])).collect();
    assert_eq!(recalled.len(), 1);
    assert!(prompts.contains(&recalled[0]) && recalled[0] != copy);
}

/// Every node holding the text fact `text`.
fn nodes_saying(fragment: &Fragment, text: &str) -> Vec<Id> {
    let handle = {
        let blob: Blob<blobencodings::UTF8String> = text.to_blob();
        blob.get_handle()
    };
    let facts = fragment.facts();
    find!(
        node: Id,
        pattern!(facts, [
            { ?node @ v3::node::holds: _?fact },
            { _?fact @ v3::fact::payload: handle },
        ])
    )
    .collect()
}

/// The one node holding `text`, asserting there are `count` in all.
fn node_saying_in(fragment: &Fragment, text: &str, count: usize) -> Id {
    let found = nodes_saying(fragment, text);
    assert_eq!(found.len(), count, "{count} node(s) hold {text:?}");
    found[0]
}

fn title(text: &str) -> Value {
    json!({"type": "ai-title", "aiTitle": text, "sessionId": "s-1"})
}

/// The nodes a title gloss with this text describes.
fn titled(fragment: &Fragment, text: &str) -> Vec<Id> {
    let handle = {
        let blob: Blob<blobencodings::UTF8String> = text.to_blob();
        blob.get_handle()
    };
    let facts = fragment.facts();
    find!(
        node: Id,
        pattern!(facts, [{ _?g @
            v3::gloss::describes: ?node,
            v3::gloss::category: (v3::gloss::category::TITLE),
            v3::gloss::text: handle,
        }])
    )
    .collect()
}

#[test]
fn a_title_describes_the_line_of_the_record_before_it_in_the_file() {
    let source = jsonl(&[
        user("u1", None, 0, "Hi."),
        assistant("a1", "u1", 1, "Hello."),
        title("Greeting"),
    ]);
    let built = built(&source).expect("builds");
    assert_eq!(
        titled(&built, "Greeting"),
        vec![node_saying(&built, "Hello.")]
    );
}

#[test]
fn a_title_before_every_record_places_nothing_only_when_restated_later() {
    let restated = jsonl(&[
        title("Greeting"),
        user("u1", None, 0, "Hi."),
        assistant("a1", "u1", 1, "Hello."),
        title("Greeting"),
    ]);
    let built = built(&restated).expect("a restatement builds");
    assert_eq!(
        titled(&built, "Greeting"),
        vec![node_saying(&built, "Hello.")]
    );

    let alone = jsonl(&[
        title("Greeting"),
        user("u1", None, 0, "Hi."),
        assistant("a1", "u1", 1, "Hello."),
    ]);
    let refused = built_err(&alone);
    assert!(refused.contains("title"), "{refused}");
}

fn built_err(source: &str) -> String {
    built(source).expect_err("refused")
}

#[test]
fn a_repeat_that_differs_only_in_machinery_is_the_same_record() {
    let mut again = user("u1", None, 0, "Hi.");
    again["cwd"] = json!("/elsewhere");
    let source = jsonl(&[
        user("u1", None, 0, "Hi."),
        again,
        assistant("a1", "u1", 1, "Hello."),
    ]);
    let once = jsonl(&[
        user("u1", None, 0, "Hi."),
        assistant("a1", "u1", 1, "Hello."),
    ]);
    assert_eq!(
        built(&source).expect("builds").facts(),
        built(&once).expect("builds").facts(),
        "a repeat differing only in the client envelope adds nothing"
    );
}

/// Along the window before `node`, root first: each held fact's (modality,
/// direction, form).
fn window_forms(fragment: &Fragment, node: Id) -> Vec<(Option<Id>, Option<Id>, Option<Id>)> {
    let facts = fragment.facts();
    let previous = |at: Id| find!(p: Id, pattern!(facts, [{ at @ v3::node::previous: ?p }])).next();
    let mut out = Vec::new();
    let mut cursor = previous(node);
    while let Some(at) = cursor {
        let m = find!(m: Id, pattern!(facts, [{ at @ v3::node::holds: _?f }, { _?f @ v3::fact::modality: ?m }])).next();
        let d = find!(d: Id, pattern!(facts, [{ at @ v3::node::holds: _?f }, { _?f @ v3::fact::direction: ?d }])).next();
        let f = find!(f: Id, pattern!(facts, [{ at @ v3::node::holds: _?x }, { _?x @ v3::fact::form: ?f }])).next();
        out.push((m, d, f));
        cursor = previous(at);
    }
    out.reverse();
    out
}

fn rewrite_transcript(sent: Value) -> String {
    let mut call = assistant("a1", "u1", 1, "unused");
    call["message"]["content"] = json!([{"type": "tool_use", "id": "t1", "name": "Read",
        "input": {"file_path": "/x"}, "caller": {"type": "direct"}}]);
    call["message"]["stop_reason"] = json!("tool_use");
    if !sent.is_null() {
        call["wireToolInputs"] = json!({"t1": sent});
    }
    let mut result = user("u2", Some("a1"), 2, "unused");
    result["message"]["content"] = json!([{"type": "tool_result", "tool_use_id": "t1",
        "content": "fine", "is_error": false}]);
    let mut done = assistant("a2", "u2", 3, "Done.");
    done["message"]["id"] = json!("m2");
    done["requestId"] = json!("req-m2");
    jsonl(&[user("u1", None, 0, "Hi."), call, result, done])
}

#[test]
fn a_call_the_client_rewrote_stands_rewritten_in_later_requests_only() {
    use v3::fact::{
        direction::{IN, OUT},
        form::REWRITTEN,
        modality::TOOL_CALL,
    };
    let built = built(&rewrite_transcript(json!({"file_path": "/y"}))).expect("builds");
    let done = node_saying(&built, "Done.");
    let later = window_forms(&built, done);
    assert!(
        later.contains(&(Some(TOOL_CALL), Some(IN), Some(REWRITTEN))),
        "the next request holds what the client sent: {later:?}"
    );
    assert!(!later.contains(&(Some(TOOL_CALL), Some(OUT), None)));
    // The result's own window holds her call as she wrote it.
    let facts = built.facts();
    let heads: Vec<Id> = find!(
        n: Id,
        pattern!(facts, [{ ?n @ v3::node::responds_to: _?call }])
    )
    .collect();
    assert_eq!(heads.len(), 1);
    let own = window_forms(&built, heads[0]);
    assert!(own.contains(&(Some(TOOL_CALL), Some(OUT), None)), "{own:?}");
    assert!(!own.iter().any(|(_, _, f)| *f == Some(REWRITTEN)));

    // A sent input the same as hers is a copy: it changes nothing.
    assert_eq!(
        built_facts(&rewrite_transcript(json!({"file_path": "/x"}))),
        built_facts(&rewrite_transcript(Value::Null))
    );
}

fn built_facts(source: &str) -> TribleSet {
    built(source).expect("builds").facts().clone()
}

fn recap(uuid: &str, parent: &str, second: u32, text: &str) -> Value {
    json!({"type": "system", "subtype": "away_summary", "uuid": uuid, "parentUuid": parent,
        "timestamp": stamp(second), "sessionId": "s-1", "content": text})
}

/// The nodes a gloss of `category` with this text describes.
fn glossed(fragment: &Fragment, category: Id, text: &str) -> Vec<Id> {
    let handle = {
        let blob: Blob<blobencodings::UTF8String> = text.to_blob();
        blob.get_handle()
    };
    let facts = fragment.facts();
    find!(
        node: Id,
        pattern!(facts, [{ _?g @
            v3::gloss::describes: ?node,
            v3::gloss::category: category,
            v3::gloss::text: handle,
        }])
    )
    .collect()
}

#[test]
fn a_recap_describes_its_parent_s_line_and_passes_the_line_on() {
    let source = jsonl(&[
        user("u1", None, 0, "Hi."),
        assistant("a1", "u1", 1, "Hello."),
        recap("s1", "a1", 2, "We said hello."),
        user("u2", Some("s1"), 3, "Again."),
    ]);
    let fragment = built(&source).expect("builds");
    let hello = node_saying(&fragment, "Hello.");
    assert_eq!(
        glossed(&fragment, v3::gloss::category::RECAP, "We said hello."),
        vec![hello]
    );
    // The summary holds no node: its child continues after the reply.
    let again = node_saying(&fragment, "Again.");
    let facts = fragment.facts();
    let before: Vec<Id> =
        find!(p: Id, pattern!(facts, [{ again @ v3::node::previous: ?p }])).collect();
    assert_eq!(before, vec![hello]);

    let orphan = jsonl(&[
        user("u1", None, 0, "Hi."),
        recap("s1", "gone", 2, "We said hello."),
    ]);
    let refused = built(&orphan).expect_err("refused");
    assert!(refused.contains("no node to describe"), "{refused}");
}

#[test]
fn an_interruption_ends_the_message_it_names_on_its_last_item() {
    let mut cut = assistant("a1", "u1", 1, "Hel");
    cut["message"]["stop_reason"] = json!(null);
    let mut stop = user("u2", Some("a1"), 2, "Stop.");
    stop["interruptedMessageId"] = json!("m1");
    let source = jsonl(&[user("u1", None, 0, "Hi."), cut, stop]);
    let fragment = built(&source).expect("builds");
    let hel = node_saying(&fragment, "Hel");
    let ends: Vec<Vec<Id>> = occurrences_of(&fragment, hel)
        .into_iter()
        .map(|o| o.2)
        .collect();
    assert_eq!(ends, vec![vec![v3::node::ends::INTERRUPTED]]);

    let mut stray = user("u2", Some("a1"), 2, "Stop.");
    stray["interruptedMessageId"] = json!("m9");
    let source = jsonl(&[
        user("u1", None, 0, "Hi."),
        assistant("a1", "u1", 1, "Hello."),
        stray,
    ]);
    let refused = built(&source).expect_err("refused");
    assert!(refused.contains("interruptedMessageId"), "{refused}");
}

fn boundary(uuid: &str, replaces: &str, second: u32) -> Value {
    json!({"type": "system", "subtype": "compact_boundary", "uuid": uuid, "parentUuid": null,
        "logicalParentUuid": replaces, "timestamp": stamp(second), "sessionId": "s-1",
        "content": "Conversation compacted", "isMeta": false, "level": "info",
        "compactMetadata": {"trigger": "auto", "preTokens": 1}})
}

#[test]
fn a_compaction_opens_a_new_root_that_continues_the_old_tail() {
    let source = jsonl(&[
        user("u1", None, 0, "Hi."),
        assistant("a1", "u1", 1, "Hello."),
        boundary("b1", "a1", 2),
        user("u2", Some("b1"), 3, "Summary."),
    ]);
    let fragment = built(&source).expect("builds");
    let summary = node_saying(&fragment, "Summary.");
    // The new context holds none of the old: the section, then the summary.
    assert_eq!(window_before(&fragment, summary), section_then(&[]));
    // Its first node off the old line continues from the old tail.
    let hello = node_saying(&fragment, "Hello.");
    let facts = fragment.facts();
    let continues: Vec<(Id, Id)> =
        find!((n: Id, t: Id), pattern!(facts, [{ ?n @ v3::node::continues: ?t }])).collect();
    assert_eq!(continues, vec![(summary, hello)]);
}

#[test]
fn a_compaction_of_a_context_outside_the_unit_opens_on_a_span_and_continues_nothing() {
    let source = jsonl(&[
        boundary("b1", "gone", 2),
        user("u2", Some("b1"), 3, "Summary."),
    ]);
    let fragment = built(&source).expect("builds");
    let summary = node_saying(&fragment, "Summary.");
    assert_eq!(
        window_before(&fragment, summary),
        section_then(&[(UNKNOWN, Some(SPAN))])
    );
    let facts = fragment.facts();
    assert_eq!(
        find!((n: Id, t: Id), pattern!(facts, [{ ?n @ v3::node::continues: ?t }])).count(),
        0
    );
}

#[test]
fn a_preserved_segment_is_re_presented_after_its_anchor() {
    let mut keep = boundary("b1", "u2", 3);
    keep["compactMetadata"]["preservedSegment"] =
        json!({"headUuid": "a1", "tailUuid": "u2", "anchorUuid": "s1"});
    let source = jsonl(&[
        user("u1", None, 0, "Hi."),
        assistant("a1", "u1", 1, "Hello."),
        user("u2", Some("a1"), 2, "Keep."),
        keep,
        user("s1", Some("b1"), 4, "Summary."),
        user("u3", Some("s1"), 5, "After."),
    ]);
    let fragment = built(&source).expect("builds");
    let after = node_saying(&fragment, "After.");
    // The summary, then the preserved segment re-presented after it.
    assert_eq!(
        window_before(&fragment, after),
        section_then(&[(TEXT, None), (TEXT, None), (TEXT, None)])
    );
    let facts = fragment.facts();
    let hellos = nodes_saying(&fragment, "Hello.");
    let keeps = nodes_saying(&fragment, "Keep.");
    assert_eq!(
        (hellos.len(), keeps.len()),
        (2, 2),
        "originals and their copies"
    );
    let recalls: Vec<(Id, Id)> =
        find!((n: Id, o: Id), pattern!(facts, [{ ?n @ v3::node::recalls: ?o }])).collect();
    assert_eq!(
        recalls.len(),
        2,
        "each copy recalls its original: {recalls:?}"
    );
}

/// A turn's own effort is a copy of the request's: equal to it, it is
/// dropped and adds nothing; different, the unit is refused where and as
/// the importer refuses it.
#[test]
fn a_mirror_that_differs_from_its_sibling_is_refused() {
    let source = |turn: Option<&str>| {
        let mut reply = assistant("a1", "u1", 1, "Hello.");
        reply["effort"] = json!("high");
        if let Some(turn) = turn {
            reply["perTurnEffort"] = json!(turn);
        }
        jsonl(&[user("u1", None, 0, "Hi."), reply])
    };
    assert_eq!(
        built_facts(&source(Some("high"))),
        built_facts(&source(None)),
        "an equal copy adds nothing"
    );
    let differs = source(Some("low"));
    let refused = built_err(&differs);
    assert_eq!(
        refused,
        "{assistant}.perTurnEffort: a mirror differs from what it mirrors"
    );
    let importer = crate::archive_v3::claude_code::import(differs.as_bytes())
        .expect_err("the importer refuses it too");
    assert_eq!(importer.to_string(), refused);
}

/// The Claude Code build of `source` by the relations, which must be the
/// importer's own: the same nodes, occurrences, links and annotations.
fn cc_exact(source: &str) -> Fragment {
    let fragment = built(source).expect("builds");
    let imported = crate::archive_v3::claude_code::import(source.as_bytes())
        .expect("the importer builds it too");
    assert_eq!(nodes_of(&fragment), nodes_of(&imported), "nodes");
    assert_eq!(
        occurrence_ids(&fragment),
        occurrence_ids(&imported),
        "occurrences"
    );
    assert_eq!(links_of(&fragment), links_of(&imported), "links");
    assert_eq!(
        annotations_of(&fragment),
        annotations_of(&imported),
        "annotations"
    );
    fragment
}

/// The Claude Code refusal of `source` by the relations, which must be the
/// importer's own, word for word.
fn cc_refused(source: &str) -> String {
    let refused = built_err(source);
    let importer = crate::archive_v3::claude_code::import(source.as_bytes())
        .expect_err("the importer refuses it too");
    assert_eq!(importer.to_string(), refused);
    refused
}

/// Her record of request `message`, holding one content block.
fn hers(uuid: &str, parent: &str, second: u32, message: &str, block: Value, stop: Value) -> Value {
    let mut record = assistant(uuid, parent, second, "unused");
    record["requestId"] = json!(format!("req-{message}"));
    record["message"]["id"] = json!(message);
    record["message"]["content"] = json!([block]);
    record["message"]["stop_reason"] = stop;
    record
}

/// His record holding `content` as it stands: a string or blocks.
fn his(uuid: &str, parent: Option<&str>, second: u32, content: Value) -> Value {
    let mut record = user(uuid, parent, second, "unused");
    record["message"]["content"] = content;
    record
}

/// An attachment the client marks rendered beside a tool result (R1).
fn beside(uuid: &str, parent: &str, second: u32) -> Value {
    json!({
        "type": "attachment", "sessionId": "s-1", "uuid": uuid, "parentUuid": parent,
        "timestamp": stamp(second), "cwd": "/synthetic", "version": "0.0.0",
        "attachment": {"type": "remote_session_change", "url": "https://example.invalid/r"},
        "rendered": [{"content": "Moved."}],
        "renderedRole": "user",
        "renderedBesideToolResult": true,
    })
}

/// Her call `t1`, as request `m1`.
fn called(uuid: &str, parent: &str, second: u32) -> Value {
    hers(
        uuid,
        parent,
        second,
        "m1",
        json!({"type": "tool_use", "id": "t1", "name": "Read", "input": {"file_path": "/x"},
            "caller": {"type": "direct"}}),
        json!("tool_use"),
    )
}

/// His record of the result of `t1`.
fn answered(uuid: &str, parent: &str, second: u32) -> Value {
    his(
        uuid,
        Some(parent),
        second,
        json!([{"type": "tool_result", "tool_use_id": "t1", "content": "x", "is_error": false}]),
    )
}

/// The R1 refusal: a beside-mark the importer does not build, and why.
fn not_beside(why: &str) -> String {
    format!("{{attachment}}.renderedBesideToolResult: {why}")
}

/// R1: a rendering marked beside a tool result, after his prompt written
/// as a plain string, past the renderings of the attachments before it, is
/// its text plus a trailing newline, as the importer builds it.
#[test]
fn a_beside_mark_after_his_plain_prompt_is_built_as_the_importer_builds_it() {
    let source = jsonl(&[
        user("u1", None, 0, "Hi."),
        beside("x1", "u1", 1),
        beside("x2", "x1", 2),
        assistant("a1", "x2", 3, "Hello."),
    ]);
    let fragment = cc_exact(&source);
    assert_eq!(nodes_saying(&fragment, "Moved.\n").len(), 2);
    assert!(nodes_saying(&fragment, "Moved.").is_empty());
    let hello = node_saying(&fragment, "Hello.");
    assert_eq!(
        window_before(&fragment, hello),
        section_then(&[(TEXT, None), (TEXT, None), (TEXT, None)])
    );
}

/// R1'': a beside-mark after harness text (`isMeta`), as blocks or as a
/// string, is refused where and as the importer refuses it. The census
/// found this one built on the local sample before the relations called
/// the importer's look-back.
#[test]
fn a_beside_mark_after_harness_text_is_refused() {
    for content in [
        json!([{"type": "text", "text": "<system-reminder>x</system-reminder>"}]),
        json!("<system-reminder>x</system-reminder>"),
    ] {
        let mut meta = his("u2", Some("a1"), 2, content);
        meta["isMeta"] = json!(true);
        let source = jsonl(&[
            user("u1", None, 0, "Hi."),
            assistant("a1", "u1", 1, "Hello."),
            meta,
            beside("x1", "u2", 3),
            hers(
                "a2",
                "x1",
                4,
                "m2",
                json!({"type": "text", "text": "Ok."}),
                json!("end_turn"),
            ),
        ]);
        assert_eq!(
            cc_refused(&source),
            not_beside(
                "rendered after harness text: what her window held there is unverified; not built"
            )
        );
    }
}

/// R1'': a beside-mark after a compaction summary is refused where and as
/// the importer refuses it.
#[test]
fn a_beside_mark_after_a_compaction_summary_is_refused() {
    let mut summary = user("c1", Some("b1"), 3, "Summary.");
    summary["isCompactSummary"] = json!(true);
    let source = jsonl(&[
        user("u1", None, 0, "First."),
        assistant("a1", "u1", 1, "Two."),
        boundary("b1", "a1", 2),
        summary,
        beside("x1", "c1", 4),
        hers(
            "a2",
            "x1",
            5,
            "m2",
            json!({"type": "text", "text": "Ok."}),
            json!("end_turn"),
        ),
    ]);
    assert_eq!(
        cc_refused(&source),
        not_beside(
            "rendered after a compaction summary: what her window held there is unverified; \
             not built"
        )
    );
}

/// R1''': a beside-mark after his prompt written as blocks, not as the
/// plain string, is refused where and as the importer refuses it.
#[test]
fn a_beside_mark_after_a_prompt_of_blocks_is_refused() {
    let source = jsonl(&[
        his("u1", None, 0, json!([{"type": "text", "text": "Hi."}])),
        beside("x1", "u1", 1),
        assistant("a1", "x1", 2, "Hello."),
    ]);
    assert_eq!(
        cc_refused(&source),
        not_beside(
            "rendered after a prompt of blocks: every real mark follows a plain string prompt; \
             not built"
        )
    );
}

/// R1: a beside-mark after a span of unknown items (its parent is outside
/// the unit) is refused where and as the importer refuses it.
#[test]
fn a_beside_mark_after_an_unknown_span_is_refused() {
    let source = jsonl(&[
        beside("x1", "outside", 1),
        assistant("a1", "x1", 2, "Hello."),
    ]);
    assert_eq!(
        cc_refused(&source),
        not_beside(
            "rendered after a span of unknown items: whether a tool result is beside it is unknown"
        )
    );
}

/// R1: a beside-mark after a tool result, its head or a part of it, past an
/// unmarked attachment's rendering, is refused where and as the importer
/// refuses it.
#[test]
fn a_beside_mark_after_a_tool_result_is_refused() {
    let mut plain = beside("x0", "u2", 3);
    plain
        .as_object_mut()
        .unwrap()
        .remove("renderedBesideToolResult");
    let refused = not_beside(
        "rendered beside a tool result: the one framed case held the text with a trailing \
         newline, not the exact text; not built",
    );
    // After its part (the result's text), past the unmarked rendering.
    let source = jsonl(&[
        user("u1", None, 0, "Read it."),
        called("a1", "u1", 1),
        answered("u2", "a1", 2),
        plain,
        beside("x1", "x0", 4),
        hers(
            "a2",
            "x1",
            5,
            "m2",
            json!({"type": "text", "text": "Read."}),
            json!("end_turn"),
        ),
    ]);
    assert_eq!(cc_refused(&source), refused);
    // After its head: a result that holds nothing.
    let mut empty = answered("u2", "a1", 2);
    empty["message"]["content"][0]["content"] = json!("");
    let source = jsonl(&[
        user("u1", None, 0, "Read it."),
        called("a1", "u1", 1),
        empty,
        beside("x1", "u2", 3),
        hers(
            "a2",
            "x1",
            4,
            "m2",
            json!({"type": "text", "text": "Read."}),
            json!("end_turn"),
        ),
    ]);
    assert_eq!(cc_refused(&source), refused);
}

/// R1': a beside-mark between a call and its result, past another marked
/// rendering, is refused where and as the importer refuses it.
#[test]
fn a_beside_mark_between_a_call_and_its_result_is_refused() {
    let source = jsonl(&[
        user("u1", None, 0, "Read it."),
        called("a1", "u1", 1),
        beside("x0", "a1", 2),
        beside("x1", "x0", 3),
        answered("u2", "x1", 4),
        hers(
            "a2",
            "u2",
            5,
            "m2",
            json!({"type": "text", "text": "Read."}),
            json!("end_turn"),
        ),
    ]);
    assert_eq!(
        cc_refused(&source),
        not_beside(
            "rendered between a call and its result: no real mark follows anything but a typed \
             prompt; not built"
        )
    );
}

/// R1'': a beside-mark after anything else (her text, her thinking, the
/// opening of a window) is refused where and as the importer refuses it.
#[test]
fn a_beside_mark_after_anything_but_his_prompt_is_refused() {
    let refused = not_beside(
        "rendered after something other than his typed prompt: no real mark stands there; not \
         built",
    );
    let source = jsonl(&[
        user("u1", None, 0, "Hi."),
        assistant("a1", "u1", 1, "Hello."),
        beside("x1", "a1", 2),
        user("u2", Some("x1"), 3, "Next."),
    ]);
    assert_eq!(cc_refused(&source), refused);
    let sealed = json!({"type": "thinking", "thinking": "", "signature": "c2lnbmVkIHRoaW5raW5n"});
    let source = jsonl(&[
        user("u1", None, 0, "Hi."),
        hers("a1", "u1", 1, "m1", sealed, Value::Null),
        beside("x1", "a1", 2),
        assistant("a2", "x1", 3, "Hello."),
    ]);
    assert_eq!(cc_refused(&source), refused);
    let mut first = beside("x1", "unused", 0);
    first["parentUuid"] = Value::Null;
    let source = jsonl(&[first, assistant("a1", "x1", 1, "Hello.")]);
    assert_eq!(cc_refused(&source), refused);
}

/// R1'': a beside-mark in the middle of one message (the message began
/// before the mark, after his prompt, and continues after it) is refused
/// where and as the importer refuses it, though the place before the mark
/// holds his prompt.
#[test]
fn a_beside_mark_in_the_middle_of_one_message_is_refused() {
    let sealed = json!({"type": "thinking", "thinking": "", "signature": "c2lnbmVkIHRoaW5raW5n"});
    let source = jsonl(&[
        user("u1", None, 0, "Hi."),
        hers("a1", "u1", 1, "m1", sealed, Value::Null),
        beside("x1", "u1", 2),
        assistant("a2", "x1", 3, "Hello."),
    ]);
    assert_eq!(
        cc_refused(&source),
        not_beside("rendered in the middle of one message: no real mark stands there; not built")
    );
}

/// S3: an attachment recorded before its rendering was is not looked back
/// from, marked or not: after her text it builds, as the importer builds
/// it. An empty rendering beside a tool result is refused on both sides,
/// but not yet in the importer's words: the image refuses the empty text
/// first (`Decode::TextThen`).
#[test]
fn a_beside_mark_the_look_back_does_not_reach() {
    let old = json!({
        "type": "attachment", "sessionId": "s-1", "uuid": "x1", "parentUuid": "a1",
        "timestamp": stamp(2), "cwd": "/synthetic",
        "attachment": {"type": "hook_success", "hookName": "SessionStart", "hookEvent": "SessionStart",
            "toolUseID": "toolu_01AbCdEfGhIjKlMnOp", "command": "echo hi", "content": "",
            "stdout": "hi\n", "stderr": "", "exitCode": 0, "durationMs": 12},
        "renderedBesideToolResult": true,
    });
    cc_exact(&jsonl(&[
        user("u1", None, 0, "Hi."),
        assistant("a1", "u1", 1, "Hello."),
        old,
        user("u2", Some("x1"), 3, "Next."),
    ]));
    let mut empty = beside("x1", "u1", 1);
    empty["rendered"] = json!([{"content": ""}]);
    let source = jsonl(&[user("u1", None, 0, "Hi."), empty]);
    built(&source).expect_err("refused");
    assert_eq!(
        crate::archive_v3::claude_code::import(source.as_bytes())
            .expect_err("the importer refuses it too")
            .to_string(),
        "{attachment}.rendered[].content: an empty rendering beside a tool result: what her \
         window held is unverified"
    );
}

// ---------------------------------------------------------------------------
// Codex: one line in file order (Order::File).

use crate::archive_v3::fields::codex::TABLE as CODEX;
use v3::fact::modality::{TOOL_CALL, TOOL_RESULT};

/// A Codex envelope.
fn envelope(second: u32, kind: &str, payload: Value) -> Value {
    json!({"timestamp": stamp(second), "type": kind, "payload": payload})
}

/// A message item: his `input_text`, or her `output_text`.
fn says(role: &str, text: &str) -> Value {
    let part = if role == "assistant" {
        "output_text"
    } else {
        "input_text"
    };
    json!({"type": "message", "role": role, "content": [{"type": part, "text": text}]})
}

fn item(second: u32, role: &str, text: &str) -> Value {
    envelope(second, "response_item", says(role, text))
}

fn event(second: u32, payload: Value) -> Value {
    envelope(second, "event_msg", payload)
}

fn codex_built(records: &[Value]) -> Result<Fragment, String> {
    built_with(&CODEX, &jsonl(records))
}

/// The turn ends claimed by the occurrences of `node`'s witnesses.
fn turn_ends_on(fragment: &Fragment, node: Id) -> Vec<Id> {
    let facts = fragment.facts();
    let mut out: Vec<Id> = find!(
        e: Id,
        pattern!(facts, [
            { _?witness @ v3::witness::projects_to: node },
            { _?occurrence @ v3::occurrence::of: _?witness, v3::witness::turn_ends: ?e },
        ])
    )
    .collect();
    out.sort();
    out
}

/// G0: a header line, then bare items. The window opens on the unrecorded
/// header, system prompt and tool definitions; the output answers the call;
/// the header line holds no node, so its claims go to the first node the
/// file places, as an occurrence of their own.
#[test]
fn codex_items_extend_one_line_after_the_unrecorded_opening() {
    let fragment = codex_built(&[
        json!({"id": "11111111-1111-4111-8111-111111111111", "timestamp": "2025-07-07T20:00:00.000Z"}),
        says("user", "List the files."),
        json!({"type": "local_shell_call", "id": "lsh_1", "call_id": "call_1", "status": "completed",
            "action": {"type": "exec", "command": ["ls"], "env": {}, "timeout_ms": 1000,
                "user": null, "working_directory": null}}),
        json!({"type": "function_call_output", "call_id": "call_1", "output": "a\nb\n"}),
        says("assistant", "Two files."),
    ])
    .expect("builds");
    let answer = node_saying(&fragment, "Two files.");
    assert_eq!(
        window_before(&fragment, answer),
        section_then(&[
            (TEXT, None),
            (TOOL_CALL, None),
            (TOOL_RESULT, None),
            (TEXT, None),
        ])
    );
    assert_eq!(links_of(&fragment).len(), 1, "the result answers the call");
    let prompt = node_saying(&fragment, "List the files.");
    assert_eq!(occurrences_of(&fragment, prompt).len(), 2);
}

fn session(second: u32, instructions: &str) -> Value {
    envelope(
        second,
        "session_meta",
        json!({"id": "33333333-3333-4333-8333-333333333333", "timestamp": stamp(second),
            "cwd": "/synthetic", "originator": "codex_cli_rs", "cli_version": "0.0.0",
            "source": "cli", "model_provider": "openai",
            "base_instructions": {"text": instructions}}),
    )
}

fn header(second: u32, effort: &str) -> Value {
    envelope(
        second,
        "turn_context",
        json!({"cwd": "/synthetic", "model": "gpt-5-codex", "effort": effort, "summary": "auto"}),
    )
}

/// A recorded system prompt and settings header open the window; a new
/// header re-presents the line under it, each copy recalling the node its
/// record held.
#[test]
fn codex_a_new_header_re_presents_the_line() {
    let fragment = codex_built(&[
        session(0, "Be brief."),
        header(1, "high"),
        item(2, "user", "Hi."),
        item(3, "assistant", "Hello."),
        header(4, "xhigh"),
        item(5, "user", "More."),
        item(6, "assistant", "Done."),
    ])
    .expect("builds");
    let done = node_saying(&fragment, "Done.");
    assert_eq!(
        window_before(&fragment, done),
        vec![
            (SYSTEM, Some(BODY)),
            (SYSTEM, None),
            (TOOL_DEFINITION, Some(SPAN)),
            (TEXT, None),
            (TEXT, None),
            (TEXT, None),
        ]
    );
    let his = nodes_saying(&fragment, "Hi.");
    assert_eq!(
        his.len(),
        2,
        "the prompt, and its copy under the new header"
    );
    let facts = fragment.facts();
    let recalled: Vec<(Id, Id)> = find!(
        (copy: Id, original: Id),
        pattern!(facts, [{ ?copy @ v3::node::recalls: ?original }])
    )
    .filter(|(copy, _)| his.contains(copy))
    .collect();
    assert_eq!(recalled.len(), 1);
    assert!(his.contains(&recalled[0].1), "the copy recalls the prompt");
    // The system prompt stands under each header; its copies recall the
    // node the session record held, under the header gap.
    let system = nodes_saying(&fragment, "Be brief.");
    assert_eq!(system.len(), 3);
    let copies: Vec<(Id, Id)> = find!(
        (copy: Id, original: Id),
        pattern!(facts, [{ ?copy @ v3::node::recalls: ?original }])
    )
    .filter(|(copy, _)| system.contains(copy))
    .collect();
    assert_eq!(copies.len(), 2);
    assert!(copies.iter().all(|(_, original)| *original == copies[0].1));
    assert!(window_before(&fragment, copies[0].1)[0] == (SYSTEM, Some(BODY)));
}

/// A rollback removes the latest turns from the line; the next items
/// follow what is left. One of more turns than the line holds is refused.
#[test]
fn codex_a_rollback_removes_the_latest_turns() {
    let turn = |second, id: &str| event(second, json!({"type": "task_started", "turn_id": id}));
    let back = |second, count: u32| {
        event(
            second,
            json!({"type": "thread_rolled_back", "num_turns": count}),
        )
    };
    let records = |count| {
        vec![
            turn(0, "t1"),
            item(1, "user", "A"),
            item(2, "assistant", "a"),
            turn(3, "t2"),
            item(4, "user", "B"),
            item(5, "assistant", "b"),
            back(6, count),
            item(7, "user", "C"),
            item(8, "assistant", "c"),
        ]
    };
    let fragment = codex_built(&records(1)).expect("builds");
    let c = node_saying(&fragment, "c");
    let text = (TEXT, None);
    assert_eq!(
        window_before(&fragment, c),
        section_then(&[text, text, text])
    );
    let b = node_saying(&fragment, "b");
    assert_eq!(
        window_before(&fragment, b),
        section_then(&[text, text, text])
    );
    let facts = fragment.facts();
    let (his, a) = (node_saying(&fragment, "C"), node_saying(&fragment, "a"));
    let before: Vec<Id> =
        find!(p: Id, pattern!(facts, [{ his @ v3::node::previous: ?p }])).collect();
    assert_eq!(before, vec![a], "C follows a, not b");
    let refused = codex_built(&records(3)).expect_err("refused");
    assert!(refused.contains("more turns"), "{refused}");
}

/// X17: where no record marks a turn, his prompt after her answer starts
/// one, so an abort after it ends no sampling of hers; it is claimed on the
/// turn's last node.
#[test]
fn codex_an_abort_after_his_new_prompt_ends_none_of_hers() {
    let abort = event(
        3,
        json!({"type": "turn_aborted", "reason": "interrupted", "turn_id": "t1"}),
    );
    let fragment = codex_built(&[
        item(0, "user", "A"),
        item(1, "assistant", "a"),
        item(2, "user", "B"),
        abort.clone(),
    ])
    .expect("builds");
    let a = node_saying(&fragment, "a");
    let b = node_saying(&fragment, "B");
    assert!(occurrences_of(&fragment, a)
        .iter()
        .all(|(_, _, ends)| ends.is_empty()));
    assert_eq!(
        turn_ends_on(&fragment, b),
        vec![v3::node::ends::INTERRUPTED]
    );

    // Marked turns: the turn holds her answer, which the abort ends.
    let fragment = codex_built(&[
        event(0, json!({"type": "task_started", "turn_id": "t1"})),
        item(0, "user", "A"),
        item(1, "assistant", "a"),
        item(2, "user", "B"),
        abort,
    ])
    .expect("builds");
    let a = node_saying(&fragment, "a");
    let b = node_saying(&fragment, "B");
    assert!(occurrences_of(&fragment, a)
        .iter()
        .any(|(_, _, ends)| ends == &vec![v3::node::ends::INTERRUPTED]));
    assert_eq!(
        turn_ends_on(&fragment, b),
        vec![v3::node::ends::INTERRUPTED]
    );
}

/// A replacement history starts the line again after the opening; its
/// first node not on the old line continues the old line's last node a
/// record holds.
#[test]
fn codex_a_compaction_starts_the_line_again_and_continues_the_old() {
    let fragment = codex_built(&[
        item(0, "user", "A"),
        item(1, "assistant", "a"),
        envelope(
            2,
            "compacted",
            json!({"message": "", "replacement_history": [
                says("user", "A"),
                says("user", "Summary."),
            ]}),
        ),
        item(3, "user", "B"),
        item(4, "assistant", "b"),
    ])
    .expect("builds");
    let b = node_saying(&fragment, "b");
    let text = (TEXT, None);
    assert_eq!(
        window_before(&fragment, b),
        section_then(&[text, text, text])
    );
    let summary = node_saying(&fragment, "Summary.");
    let a = node_saying(&fragment, "a");
    let facts = fragment.facts();
    let continued: Vec<Id> =
        find!(t: Id, pattern!(facts, [{ summary @ v3::node::continues: ?t }])).collect();
    assert_eq!(continued, vec![a]);

    // The old line is gone: a header after the compaction re-presents only
    // the new one.
    let fragment = codex_built(&[
        item(0, "user", "A"),
        item(1, "assistant", "a"),
        envelope(
            2,
            "compacted",
            json!({"message": "", "replacement_history": [says("user", "Summary.")]}),
        ),
        header(3, "high"),
        item(4, "user", "B"),
        item(5, "assistant", "b"),
    ])
    .expect("builds");
    let b = node_saying(&fragment, "b");
    assert_eq!(
        window_before(&fragment, b),
        vec![
            (SYSTEM, Some(BODY)),
            (SYSTEM, Some(BODY)),
            (TOOL_DEFINITION, Some(SPAN)),
            text,
            text,
        ]
    );
}

/// A result after a header change answers the call as re-presented under
/// the new header; one whose latest call a rollback removed is refused,
/// even when an earlier call with its id still stands (X4).
#[test]
fn codex_a_result_answers_its_latest_call_on_the_line() {
    let call = |second, id: &str| {
        envelope(
            second,
            "response_item",
            json!({"type": "function_call", "name": "f", "arguments": "{}", "call_id": id}),
        )
    };
    let output = |second, id: &str| {
        envelope(
            second,
            "response_item",
            json!({"type": "function_call_output", "call_id": id, "output": "ok"}),
        )
    };
    let fragment = codex_built(&[
        header(0, "high"),
        item(1, "user", "Go."),
        call(2, "c1"),
        header(3, "low"),
        output(4, "c1"),
    ])
    .expect("builds");
    // The result's head (before its text) answers the call right before
    // it: the copy under the low header.
    let text = node_saying(&fragment, "ok");
    let facts = fragment.facts();
    let previous = |at: Id| -> Vec<Id> {
        find!(p: Id, pattern!(facts, [{ at @ v3::node::previous: ?p }])).collect()
    };
    let head = previous(text)[0];
    assert_eq!(held_modality(&fragment, head), TOOL_RESULT);
    let answered: Vec<Id> =
        find!(t: Id, pattern!(facts, [{ head @ v3::node::responds_to: ?t }])).collect();
    assert_eq!(answered, previous(head));
    assert_eq!(
        window_before(&fragment, answered[0])[0],
        (SYSTEM, Some(BODY))
    );

    let turn = |second, id: &str| event(second, json!({"type": "task_started", "turn_id": id}));
    let refused = codex_built(&[
        turn(0, "t1"),
        item(1, "user", "Go."),
        call(2, "a"),
        turn(3, "t2"),
        call(4, "a"),
        event(5, json!({"type": "thread_rolled_back", "num_turns": 1})),
        output(6, "a"),
    ])
    .expect_err("refused");
    assert!(refused.contains("not in its window"), "{refused}");
}

/// A record that holds no node claims for the next node placed when it
/// opens the window (a header, a system prompt) or bears no item at all;
/// an item whose values rendered nothing claims for none.
#[test]
fn codex_claims_of_a_record_that_holds_nothing() {
    // An empty reply: its time is no claim of the next node.
    let fragment =
        codex_built(&[item(0, "assistant", ""), item(1, "user", "Hi.")]).expect("builds");
    let hi = node_saying(&fragment, "Hi.");
    assert_eq!(occurrences_of(&fragment, hi).len(), 1);

    // A header with no settings item: its claims, empty or not, go to the
    // next node, and a file that ends before one is refused (X19).
    let bare = envelope(1, "turn_context", json!({"cwd": "/synthetic"}));
    let fragment =
        codex_built(&[item(0, "user", "A"), bare.clone(), item(2, "user", "B")]).expect("builds");
    let b = node_saying(&fragment, "B");
    assert_eq!(occurrences_of(&fragment, b).len(), 2);
    let refused = codex_built(&[item(0, "user", "A"), bare]).expect_err("refused");
    assert!(refused.contains("beyond the end of the file"), "{refused}");
}

/// The Codex refusal of `records` by the relations, which must be the
/// importer's own, word for word.
fn codex_refused(records: &[Value]) -> String {
    let refused = codex_built(records).expect_err("refused");
    let importer = crate::archive_v3::codex::import(jsonl(records).as_bytes())
        .expect_err("the importer refuses it too");
    assert_eq!(importer.to_string(), refused);
    refused
}

/// Her text, as output text parts of one message.
fn her_parts(second: u32, parts: &[&str]) -> Value {
    let content: Vec<Value> = parts
        .iter()
        .map(|text| json!({"type": "output_text", "text": text}))
        .collect();
    envelope(
        second,
        "response_item",
        json!({"type": "message", "role": "assistant", "content": content}),
    )
}

/// A client copy of her text is dropped where its own unit holds a value
/// equal to it at a path the mirror names, anywhere in the file, and adds
/// nothing. Otherwise the unit is refused where and as the importer refuses
/// it.
#[test]
fn codex_a_copy_is_dropped_only_where_its_own_unit_holds_its_own_value() {
    let copy =
        |second, text: &str| event(second, json!({"type": "agent_message", "message": text}));
    let unheld =
        "{event_msg}.payload{agent_message}.message: a copy the unit does not hold what it copies";
    let (hi, hello) = (item(0, "user", "Hi."), item(1, "assistant", "Hello."));
    let plain = codex_built(&[hi.clone(), hello.clone()])
        .expect("builds")
        .facts()
        .clone();
    for records in [
        vec![hi.clone(), hello.clone(), copy(2, "Hello.")],
        // No order is involved: a copy may come before what it copies.
        vec![copy(0, "Hello."), hi.clone(), hello.clone()],
    ] {
        let fragment = codex_built(&records).expect("builds");
        assert_eq!(fragment.facts(), &plain, "the copy adds nothing");
        node_saying_in(&fragment, "Hello.", 1);
    }
    // A value the unit does not hold.
    assert_eq!(
        codex_refused(&[hi.clone(), hello.clone(), copy(2, "Goodbye.")]),
        unheld
    );
    // A held copy backs no other copy of the same mirror.
    assert_eq!(
        codex_refused(&[
            hi.clone(),
            hello.clone(),
            copy(2, "Hello."),
            copy(3, "Goodbye."),
        ]),
        unheld
    );
    // A value the unit holds at no path the mirror names: his text.
    assert_eq!(
        codex_refused(&[hi.clone(), hello.clone(), copy(2, "Hi.")]),
        unheld
    );
    // An original that holds two values backs each of them, never the two
    // as one.
    let two = her_parts(1, &["Hello.", "Again."]);
    codex_built(&[hi.clone(), two.clone(), copy(2, "Again.")]).expect("builds");
    assert_eq!(
        codex_refused(&[hi.clone(), two, copy(2, "Hello.Again.")]),
        unheld
    );
    // What one unit holds backs nothing in the next: built one after the
    // other, the unit without the original is refused.
    codex_built(&[item(0, "assistant", "Hello."), copy(1, "Hello.")]).expect("builds");
    assert_eq!(codex_refused(&[hi, copy(1, "Hello.")]), unheld);
}

/// A client copy of part of his text is held as an exact substring of a
/// value at a path the mirror names, and refused otherwise.
#[test]
fn codex_a_copy_within_his_text_is_held_only_inside_it() {
    let copy = |second, text: &str| event(second, json!({"type": "user_message", "message": text}));
    let unheld =
        "{event_msg}.payload{user_message}.message: a copy the unit does not hold what it copies";
    let his = item(0, "user", "Fix the bug, please.");
    codex_built(&[his.clone(), copy(1, "Fix the bug")]).expect("builds");
    // Exact: case counts.
    assert_eq!(
        codex_refused(&[his.clone(), copy(1, "fix the bug")]),
        unheld
    );
    // Inside her text, which the mirror does not name.
    assert_eq!(
        codex_refused(&[his, item(1, "assistant", "Fixed it."), copy(2, "Fixed it")]),
        unheld
    );
}

/// A header's collaboration settings copy its own fields: equal, they are
/// dropped and add nothing; different, the unit is refused where and as the
/// importer refuses it.
#[test]
fn codex_a_mirror_that_differs_from_its_record_s_field_is_refused() {
    let collaborating = |effort: &str| {
        envelope(
            1,
            "turn_context",
            json!({"cwd": "/synthetic", "model": "gpt-5-codex", "effort": "high",
                "summary": "auto", "collaboration_mode": {"mode": "default",
                    "settings": {"model": "gpt-5-codex", "reasoning_effort": effort}}}),
        )
    };
    let records = |header: Value| {
        vec![
            item(0, "user", "Hi."),
            header,
            item(2, "assistant", "Hello."),
        ]
    };
    assert_eq!(
        codex_built(&records(collaborating("high")))
            .expect("builds")
            .facts(),
        codex_built(&records(header(1, "high")))
            .expect("builds")
            .facts(),
        "an equal copy adds nothing"
    );
    assert_eq!(
        codex_refused(&records(collaborating("low"))),
        "{turn_context}.payload.collaboration_mode.settings.reasoning_effort: a mirror differs \
         from what it mirrors"
    );
}

/// The Codex build of `records` by the relations, which must be the
/// importer's own: the same nodes, occurrences, links and annotations.
fn codex_exact(records: &[Value]) -> Fragment {
    let fragment = codex_built(records).expect("builds");
    let imported = crate::archive_v3::codex::import(jsonl(records).as_bytes())
        .expect("the importer builds it too");
    assert_eq!(nodes_of(&fragment), nodes_of(&imported), "nodes");
    assert_eq!(
        occurrence_ids(&fragment),
        occurrence_ids(&imported),
        "occurrences"
    );
    assert_eq!(links_of(&fragment), links_of(&imported), "links");
    assert_eq!(
        annotations_of(&fragment),
        annotations_of(&imported),
        "annotations"
    );
    assert_eq!(
        untruncated_of(&fragment),
        untruncated_of(&imported),
        "untruncated claims"
    );
    fragment
}

/// The full texts occurrences claim, as (occurrence, text handle) pairs
/// (X18).
fn untruncated_of(fragment: &Fragment) -> std::collections::BTreeSet<(Id, [u8; 32])> {
    let facts = fragment.facts();
    find!(
        (o: Id, text: Name),
        pattern!(facts, [{ ?o @ v3::witness::untruncated: ?text }])
    )
    .map(|(o, text)| (o, text.raw))
    .collect()
}

/// The occurrences that claim `text` as the full text of an output her
/// window held cut (X18): one per node its record holds, the result's head
/// and the cut text, as the importer gives each its record's claims.
fn claiming_untruncated(fragment: &Fragment, text: &str) -> usize {
    let handle = {
        let blob: Blob<blobencodings::UTF8String> = text.to_blob();
        blob.get_handle()
    };
    let facts = fragment.facts();
    find!(o: Id, pattern!(facts, [{ ?o @ v3::witness::untruncated: handle }])).count()
}

/// One tool output under a header that records `policy` (none: no budget),
/// in a unit a client of `version` wrote; `metadata` is the output
/// record's own.
fn budgeted(
    version: &str,
    policy: Option<Value>,
    output: Value,
    metadata: Option<Value>,
) -> Vec<Value> {
    let mut opened = session(0, "Sys.");
    opened["payload"]["cli_version"] = json!(version);
    let mut context = header(1, "high");
    if let Some(policy) = policy {
        context["payload"]["truncation_policy"] = policy;
    }
    let mut out = envelope(
        4,
        "response_item",
        json!({"type": "custom_tool_call_output", "call_id": "o1", "output": output}),
    );
    if let Some(metadata) = metadata {
        out["metadata"] = metadata;
    }
    vec![
        opened,
        context,
        item(2, "user", "Go."),
        envelope(
            3,
            "response_item",
            json!({"type": "custom_tool_call", "name": "exec", "input": "ls", "call_id": "o1"}),
        ),
        out,
        item(5, "assistant", "Done."),
    ]
}

/// X18: her window holds a tool output as her client's history cut it
/// under the latest header's budget, in bytes or in tokens with codex's 20%
/// allowance, or under the record's own limit; the full text is the
/// record's claim. An output within the budget, exactly at it too, is
/// whole and claims nothing.
#[test]
fn codex_a_budget_cuts_an_output_to_what_her_window_held() {
    let x = |n: usize| json!("x".repeat(n));
    let bytes = |n: u64| Some(json!({"mode": "bytes", "limit": n}));
    let tokens = |n: u64| Some(json!({"mode": "tokens", "limit": n}));
    // Bytes 10 allow 12: 13 bytes keep 6 and 6, with a character count.
    let cut = codex_exact(&budgeted("0.0.0", bytes(10), x(13), None));
    node_saying_in(&cut, "xxxxxx\u{2026}1 chars truncated\u{2026}xxxxxx", 1);
    assert!(nodes_saying(&cut, &"x".repeat(13)).is_empty());
    assert_eq!(claiming_untruncated(&cut, &"x".repeat(13)), 2);
    // Exactly at the allowance: whole.
    let whole = codex_exact(&budgeted("0.0.0", bytes(10), x(12), None));
    node_saying_in(&whole, &"x".repeat(12), 1);
    assert_eq!(claiming_untruncated(&whole, &"x".repeat(12)), 0);
    // Tokens 2 allow 3, 12 bytes; the cut keeps whole characters.
    let cut = codex_exact(&budgeted(
        "0.0.0",
        tokens(2),
        json!("\u{e9}".repeat(7)),
        None,
    ));
    node_saying_in(
        &cut,
        "\u{e9}\u{e9}\u{e9}\u{2026}1 tokens truncated\u{2026}\u{e9}\u{e9}\u{e9}",
        1,
    );
    assert_eq!(claiming_untruncated(&cut, &"\u{e9}".repeat(7)), 2);
    let whole = codex_exact(&budgeted("0.0.0", tokens(2), x(12), None));
    node_saying_in(&whole, &"x".repeat(12), 1);
    // A budget of zero shows only the marker.
    let cut = codex_exact(&budgeted("0.0.0", bytes(0), x(5), None));
    node_saying_in(&cut, "\u{2026}5 chars truncated\u{2026}", 1);
    // A record's own limit is in tokens and holds its allowance already.
    let limit = |n: u64| Some(json!({"fallback_token_limit_override": n}));
    let cut = codex_exact(&budgeted("0.0.0", bytes(10), x(13), limit(3)));
    node_saying_in(&cut, "xxxxxx\u{2026}1 tokens truncated\u{2026}xxxxxx", 1);
    let whole = codex_exact(&budgeted("0.0.0", bytes(1), x(13), limit(100)));
    node_saying_in(&whole, &"x".repeat(13), 1);
}

/// A header that records no budget carries none from an earlier header:
/// the output after it is whole, as without any budget.
#[test]
fn codex_a_header_without_a_budget_carries_none_into_its_turn() {
    let x13 = json!("x".repeat(13));
    let unbudgeted = codex_exact(&budgeted("0.0.0", None, x13.clone(), None));
    node_saying_in(&unbudgeted, &"x".repeat(13), 1);
    assert_eq!(claiming_untruncated(&unbudgeted, &"x".repeat(13)), 0);
    let mut records = budgeted("0.0.0", None, x13, None);
    let mut earlier = header(1, "high");
    earlier["payload"]["truncation_policy"] = json!({"mode": "bytes", "limit": 10});
    records.splice(
        1..1,
        [
            earlier,
            item(1, "user", "Hi."),
            item(1, "assistant", "Hello."),
        ],
    );
    let fragment = codex_exact(&records);
    node_saying_in(&fragment, &"x".repeat(13), 1);
    assert_eq!(claiming_untruncated(&fragment, &"x".repeat(13)), 0);
}

/// A budget the importer cannot apply, and a list output a budget would
/// shorten, are refused where and as the importer refuses them.
#[test]
fn codex_a_budget_the_importer_cannot_apply_is_refused() {
    let x13 = json!("x".repeat(13));
    for bad in [
        json!(null),
        json!({"mode": "lines", "limit": 1}),
        json!({"mode": "bytes", "limit": 1.5}),
        json!({"mode": "bytes", "limit": "1"}),
        json!({"mode": "bytes", "limit": -1}),
        json!({"mode": "bytes"}),
        json!({"mode": "bytes", "limit": 1, "extra": 1}),
    ] {
        assert_eq!(
            codex_refused(&budgeted("0.0.0", Some(bad), x13.clone(), None)),
            "{turn_context}.payload.truncation_policy: a history budget that does not parse: \
             the importer cannot apply it"
        );
    }
    let bytes = Some(json!({"mode": "bytes", "limit": 10}));
    assert_eq!(
        codex_refused(&budgeted(
            "0.0.0",
            bytes.clone(),
            x13.clone(),
            Some(json!({"fallback_token_limit_override": "3"}))
        )),
        "{response_item}.metadata.fallback_token_limit_override: a history budget override \
         that is no count"
    );
    let parts = |texts: &[String]| {
        let parts: Vec<Value> = texts
            .iter()
            .map(|text| json!({"type": "input_text", "text": text}))
            .collect();
        json!(parts)
    };
    // 8 and 4 bytes fit the 12 allowed; 8 and 5 do not, and once the
    // budget is spent an empty part is omitted too.
    codex_exact(&budgeted(
        "0.0.0",
        bytes.clone(),
        parts(&["x".repeat(8), "y".repeat(4)]),
        None,
    ));
    let shortened = "{response_item}.payload{custom_tool_call_output}.output: a list output its \
                     history budget shortens: how to keep its full parts beside her window is \
                     not modeled";
    for texts in [
        ["x".repeat(8), "y".repeat(5)],
        ["x".repeat(12), String::new()],
    ] {
        assert_eq!(
            codex_refused(&budgeted("0.0.0", bytes.clone(), parts(&texts), None)),
            shortened
        );
    }
}

/// X18': where no budget is recorded, an output a budget her client could
/// have run under would cut is refused where and as the importer refuses
/// it: 0.59 on at 12,000 bytes, 0.51 to 0.58 at 10 KiB or 256 lines, an
/// unknown client at either. At the bound, and before 0.51, it is whole.
/// A bare item predates the clients that cut.
#[test]
fn codex_an_output_a_client_could_have_cut_unrecorded_is_refused() {
    let x = |n: usize| json!("x".repeat(n));
    let unknown = "{response_item}.payload{custom_tool_call_output}.output: an output a budget \
                   her client could have run under cuts, and the rollout records none: the \
                   truncation is unrecorded, so what her window held is unknown (X18')";
    for (version, output) in [
        ("0.140.0", x(13_000)),
        ("0.70.0", x(13_000)),
        ("0.55.0", x(11_000)),
        ("0.55.0", json!("x\n".repeat(257))),
        ("not a version", x(11_000)),
        (
            "0.140.0",
            json!([{"type": "input_text", "text": "x".repeat(13_000)}]),
        ),
    ] {
        assert_eq!(
            codex_refused(&budgeted(version, None, output, None)),
            unknown
        );
    }
    for (version, n) in [("0.140.0", 12_000), ("0.55.0", 10_240), ("0.50.0", 50_000)] {
        let fragment = codex_exact(&budgeted(version, None, x(n), None));
        node_saying_in(&fragment, &"x".repeat(n), 1);
    }
    // A recorded budget is what her window held: no refusal.
    codex_exact(&budgeted(
        "0.140.0",
        Some(json!({"mode": "bytes", "limit": 20_000})),
        x(13_000),
        None,
    ));
    let long = "x".repeat(50_000);
    let bare = [
        json!({"id": "11111111-1111-4111-8111-111111111111", "timestamp": "2025-07-07T20:00:00.000Z"}),
        says("user", "List the files."),
        json!({"type": "function_call", "name": "shell", "arguments": "{}", "call_id": "call_1"}),
        json!({"type": "function_call_output", "call_id": "call_1", "output": long}),
        says("assistant", "Done."),
    ];
    node_saying_in(&codex_built(&bare).expect("builds"), &long, 1);
    crate::archive_v3::codex::import(jsonl(&bare).as_bytes()).expect("the importer builds it");
}

/// The client's copy of her message's phase (X1): an `agent_message`
/// event.
fn phase_copy(second: u32, text: &str, phase: &str) -> Value {
    event(
        second,
        json!({"type": "agent_message", "message": text, "phase": phase}),
    )
}

/// Her message, with its own phase or none.
fn her_phased(second: u32, text: &str, phase: Option<&str>) -> Value {
    let mut said = says("assistant", text);
    if let Some(phase) = phase {
        said["phase"] = json!(phase);
    }
    envelope(second, "response_item", said)
}

/// Her sealed reasoning, which codex writes between a copy and its message.
fn reasoning(second: u32) -> Value {
    envelope(
        second,
        "response_item",
        json!({"type": "reasoning", "summary": [], "content": null,
            "encrypted_content": "c2VhbGVkIHJlYXNvbmluZyA_Pn4g-_8="}),
    )
}

/// The registers of the facts the nodes holding `text` hold (X1).
fn registers_saying(fragment: &Fragment, text: &str) -> Vec<Id> {
    let handle = {
        let blob: Blob<blobencodings::UTF8String> = text.to_blob();
        blob.get_handle()
    };
    let facts = fragment.facts();
    let found: std::collections::BTreeSet<Id> = find!(
        r: Id,
        pattern!(facts, [
            { _?node @ v3::node::holds: _?fact },
            { _?fact @ v3::fact::payload: handle, v3::fact::register: ?r },
        ])
    )
    .collect();
    found.into_iter().collect()
}

/// A session and header, then his question.
fn asked() -> Vec<Value> {
    vec![session(0, "Sys."), header(1, "high"), item(2, "user", "Q.")]
}

/// X1, X13: her message that holds no phase takes its event copy's as the
/// register of every text it holds, the copy naming the first item after it
/// past her reasoning. Without the copy her text holds no register.
#[test]
fn codex_a_phase_copy_supplies_the_register_of_the_message_after_it() {
    use v3::fact::register::{ANSWER, COMMENTARY};
    for (phase, register) in [("commentary", COMMENTARY), ("final_answer", ANSWER)] {
        let mut records = asked();
        records.extend([
            phase_copy(3, "Looking.", phase),
            reasoning(3),
            her_phased(3, "Looking.", None),
        ]);
        let fragment = codex_exact(&records);
        node_saying_in(&fragment, "Looking.", 1);
        assert_eq!(registers_saying(&fragment, "Looking."), vec![register]);
    }
    // Every text of her message, the copy matching one of them.
    let mut records = asked();
    records.extend([
        phase_copy(3, "Looking.", "commentary"),
        her_parts(3, &["Looking.", "More."]),
    ]);
    let fragment = codex_exact(&records);
    assert_eq!(registers_saying(&fragment, "Looking."), vec![COMMENTARY]);
    assert_eq!(registers_saying(&fragment, "More."), vec![COMMENTARY]);
    let mut records = asked();
    records.extend([reasoning(3), her_phased(3, "Looking.", None)]);
    let fragment = codex_exact(&records);
    assert!(registers_saying(&fragment, "Looking.").is_empty());
}

/// Where her message holds its own phase, a copy that agrees adds nothing:
/// her text's register is her message's own.
#[test]
fn codex_a_phase_copy_that_agrees_with_her_message_adds_nothing() {
    let mut records = asked();
    records.push(her_phased(3, "Looking.", Some("commentary")));
    let plain = codex_exact(&records);
    records.insert(3, phase_copy(3, "Looking.", "commentary"));
    let fragment = codex_exact(&records);
    assert_eq!(fragment.facts(), plain.facts(), "the copy adds nothing");
    assert_eq!(
        registers_saying(&fragment, "Looking."),
        vec![v3::fact::register::COMMENTARY]
    );
}

/// A copy that disagrees with her message's phase, or with another copy
/// of the same message, is refused where and as the importer refuses it.
#[test]
fn codex_a_phase_copy_that_disagrees_is_refused() {
    let disagrees =
        "{event_msg}.payload{agent_message}.phase: a phase copy that disagrees with her message's \
         phase";
    let mut records = asked();
    records.extend([
        phase_copy(3, "Looking.", "commentary"),
        her_phased(3, "Looking.", Some("final_answer")),
    ]);
    assert_eq!(codex_refused(&records), disagrees);
    let mut records = asked();
    records.extend([
        phase_copy(3, "Looking.", "commentary"),
        phase_copy(3, "Looking.", "final_answer"),
        her_phased(3, "Looking.", None),
    ]);
    assert_eq!(codex_refused(&records), disagrees);
}

/// X13, X13': a copy names its message by structure. One whose message is
/// not the item after it, though another record holds its text, is matched
/// by text alone; one whose message only a compaction's replacement
/// history holds has no structural match. Both are refused where and as the
/// importer refuses them.
#[test]
fn codex_a_phase_copy_matched_by_text_alone_is_refused() {
    let alone = "{event_msg}.payload{agent_message}.phase: a phase copy whose message is not the \
                 item after it: matched by text alone";
    let mut records = asked();
    records.extend([
        her_phased(3, "Done.", None),
        phase_copy(4, "Done.", "commentary"),
    ]);
    assert_eq!(codex_refused(&records), alone);
    let mut records = asked();
    records.extend([
        phase_copy(3, "Done.", "commentary"),
        item(4, "user", "More."),
        her_phased(5, "Done.", None),
    ]);
    assert_eq!(codex_refused(&records), alone);
    let mut records = asked();
    records.extend([
        her_phased(3, "Done.", None),
        phase_copy(4, "Kept.", "commentary"),
        envelope(
            5,
            "compacted",
            json!({"message": "", "replacement_history": [
                says("assistant", "Kept."),
                {"type": "compaction", "encrypted_content": "c2VhbGVkIGNvbXBhY3Rpb24g--8="},
            ]}),
        ),
        her_phased(6, "After.", None),
    ]);
    assert_eq!(
        codex_refused(&records),
        "{event_msg}.payload{agent_message}.phase: a phase copy whose message only a \
         replacement history holds: no structural match"
    );
}

/// A copy whose text no record holds (a fork's copied prefix, X8') holds
/// nothing: the message after it takes no register, and the unit builds.
#[test]
fn codex_a_phase_copy_whose_text_no_record_holds_holds_nothing() {
    let records = |copy: bool| {
        let mut forked = session(0, "Sys.");
        forked["payload"]["forked_from_id"] = json!("99999999-9999-4999-8999-999999999999");
        let mut records = vec![forked, header(1, "high"), item(2, "user", "Parent asks.")];
        if copy {
            records.push(phase_copy(2, "Parent comments.", "commentary"));
        }
        records.extend([
            item(3, "user", "Child task."),
            reasoning(4),
            her_phased(5, "Done.", None),
        ]);
        records
    };
    let fragment = codex_exact(&records(true));
    assert!(nodes_saying(&fragment, "Parent comments.").is_empty());
    assert!(registers_saying(&fragment, "Done.").is_empty());
    assert_eq!(
        fragment.facts(),
        codex_exact(&records(false)).facts(),
        "the copy adds nothing"
    );
}

/// A phase the table does not name, on a copy or on her message, is
/// refused where and as the importer refuses it.
#[test]
fn codex_a_phase_the_table_does_not_name_is_refused() {
    let mut records = asked();
    records.extend([
        phase_copy(3, "Looking.", "aside"),
        her_phased(3, "Looking.", None),
    ]);
    assert_eq!(
        codex_refused(&records),
        "{event_msg}.payload{agent_message}.phase: a vocabulary value the table does not name"
    );
    // Her own message's unnamed phase, no copy: refused on both sides, but
    // not yet in the importer's words. The image refuses the value before
    // the relations read it, and says so in its construction findings.
    let mut records = asked();
    records.push(her_phased(3, "Looking.", Some("aside")));
    codex_built(&records).expect_err("refused");
    crate::archive_v3::codex::import(jsonl(&records).as_bytes())
        .expect_err("the importer refuses it too");
}
