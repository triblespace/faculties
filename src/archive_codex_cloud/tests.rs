use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Value};
use triblespace::core::repo::{BlobStoreGet, SnapshotSource};
use triblespace::prelude::blobencodings::UTF8String;
use triblespace::prelude::inlineencodings::Handle;

use super::*;
use crate::blockdag;
use schema::source_projection::SOURCE_CODEX_CLOUD;

const TASK_ID: &str = "task_e_68a0000000000000000000000000000a";

fn turn_id(kind: &str, second: u32, serial: u32) -> String {
    format!("{TASK_ID}~{kind}_e_{second:08x}{serial:024x}")
}

fn diff(inline: Option<&str>, external: Option<&str>) -> Value {
    json!({
        "additional_parent_commit_shas": null,
        "base_commit_sha": "0123456789abcdef0123456789abcdef01234567",
        "commit_message": "Round the corners",
        "diff": inline,
        "external_storage_diff": external.map(|id| json!({"file_id": id, "ttl": null})),
        "files_modified": 2,
        "lines_added": 7,
        "lines_removed": 1,
        "preapply_patch_base_commit_sha": null,
        "repo_id": "example/widget",
        "type": "output_diff",
    })
}

fn bare_diff(external: &str) -> Value {
    let mut bare = diff(None, Some(external));
    for name in [
        "base_commit_sha",
        "commit_message",
        "files_modified",
        "lines_added",
        "lines_removed",
        "repo_id",
    ] {
        bare[name] = Value::Null;
    }
    bare
}

fn text(value: &str) -> Value {
    json!({"content_type": "text", "text": value})
}

fn message(role: &str, content: Vec<Value>) -> Value {
    json!({"type": "message", "role": role, "content": content})
}

fn user(serial: u32, items: Vec<Value>) -> Value {
    json!({
        "custom_instructions": null,
        "id": turn_id("usertrn", 0x68a0_0000 + serial, serial),
        "input_items": items,
        "role": "user",
    })
}

fn assistant(serial: u32, items: Vec<Value>) -> Value {
    json!({
        "branch": "main",
        "branch_name": null,
        "external_pull_request_id": "None",
        "id": turn_id("assttrn", 0x68a0_0000 + serial, serial + 1000),
        "output_items": items,
        "previous_turn_id": turn_id("usertrn", 0x68a0_0000 + serial, serial),
        "pull_request_status": "not_created",
        "role": "assistant",
        "turn_status": "TaskTurnStatusEnum.COMPLETED",
    })
}

/// A task that holds every item kind the importer knows.
fn task() -> Value {
    json!({
        "archived": true,
        "id": TASK_ID,
        "title": "A title the task list shows",
        "turns": [
            user(1, vec![
                json!({"type": "ide_context", "context": "open file: widget.rs"}),
                message("user", vec![text("Please round the widget's corners.")]),
                json!({"type": "pre_apply_patch", "output_diff": bare_diff("file_00000000000000000000000000000001")}),
                json!({
                    "type": "prior_conversation",
                    "diff": null,
                    "prior_task_id": null,
                    "conversation": [
                        message("user", vec![text("Are the corners square?")]),
                        message("assistant", vec![text("They are.")]),
                    ],
                }),
            ]),
            assistant(1, vec![
                json!({
                    "type": "pr",
                    "output_diff": diff(Some("--- a/w\n+++ b/w\n"), None),
                    "pr_message": "Round them.\n\nTested.",
                    "pr_title": "Round corners",
                    "pre_apply_patch": null,
                }),
                json!({"type": "follow_up_diff", "output_diff": diff(None, Some("file_00000000000000000000000000000002"))}),
                message("assistant", vec![
                    text("I rounded them"),
                    json!({"content_type": "repo_file_citation", "path": "w.rs", "line_range_start": 3, "line_range_end": null}),
                    text(" and ran the tests"),
                    json!({"content_type": "terminal_chunk_citation", "terminal_chunk_id": "0a1b2c", "line_range_start": 1, "line_range_end": 4}),
                ]),
                json!({"type": "partial_repo_snapshot", "files": [{"path": "w.rs"}]}),
            ]),
            user(2, vec![
                message("user", vec![text("")]),
                json!({
                    "type": "comment",
                    "content": [text("This one is still square.")],
                    "position": {"line": 12, "path": "w.rs", "side": "right", "start_line": null},
                }),
            ]),
            assistant(2, vec![]),
        ],
    })
}

type Datum = (Id, Id, String);

struct Projected {
    summary: ProjectionSummary,
    facts: TribleSet,
    nodes: Vec<Id>,
    data: Vec<Datum>,
}

fn project(tasks: &Value) -> Result<Projected> {
    let mut union = Fragment::empty();
    let mut nodes = Vec::new();
    let summary = project_bytes(
        "codex.json",
        Bytes::from_source(serde_json::to_vec(tasks).unwrap()),
        |projected| {
            nodes.extend(projected.identity.nodes().iter().copied());
            union += projected.fragment;
            Ok(())
        },
    )?;
    let (_, facts, _, mut blobs) = union.into_parts();
    let reader = blobs.snapshot().unwrap();
    let data = nodes
        .iter()
        .map(|node| {
            find!(
                (kind: Id, facing: Id, value: Inline<Handle<UTF8String>>),
                pattern!(&facts, [
                    { *node @ schema::block::holds: _?fact },
                    { _?fact @
                        schema::content_fact::modality: ?kind,
                        schema::content_fact::direction: ?facing,
                        schema::content_fact::payload: ?value },
                ])
            )
            .chain(find!(
                (kind: Id, facing: Id, value: Inline<Handle<UTF8String>>),
                pattern!(&facts, [
                    { *node @ schema::block::holds: _?fact },
                    { _?fact @
                        schema::content_fact::modality: ?kind,
                        schema::content_fact::direction: ?facing,
                        schema::content_fact::asset_pointer: ?value },
                ])
            ))
            .map(|(kind, facing, value)| {
                let value: View<str> = reader.get(value).unwrap();
                (kind, facing, value.as_ref().to_owned())
            })
            .next()
            .expect("every node holds one fact")
        })
        .collect();
    Ok(Projected {
        summary,
        facts,
        nodes,
        data,
    })
}

// ---------------------------------------------------------------------------
// The gate: a second reading of the export over a dynamic JSON tree
// ---------------------------------------------------------------------------
//
// It shares no code with the importer. It takes every value it expects to be
// projected out of the tree, so what is left in the tree afterwards is exactly
// what nothing rendered, and that is counted by path.

/// Paths that may be left in the tree: structure (types, roles, ids that
/// thread the turns) and what the shapes leave out by name.
const UNRENDERED: &[&str] = &[
    ".archived",
    ".id",
    ".title",
    ".turns[].branch",
    ".turns[].branch_name",
    ".turns[].external_pull_request_id",
    ".turns[].id",
    ".turns[].input_items[].content[].content_type",
    ".turns[].input_items[].content[].text", // only the empty text
    ".turns[].input_items[].conversation[].content[].content_type",
    ".turns[].input_items[].conversation[].role",
    ".turns[].input_items[].conversation[].type",
    ".turns[].input_items[].output_diff.type",
    ".turns[].input_items[].role",
    ".turns[].input_items[].type",
    ".turns[].output_items[].content[].content_type",
    ".turns[].output_items[].files",
    ".turns[].output_items[].output_diff.type",
    ".turns[].output_items[].role",
    ".turns[].output_items[].type",
    ".turns[].previous_turn_id",
    ".turns[].pull_request_status",
    ".turns[].role",
    ".turns[].turn_status",
];

fn taken_text(value: &mut Value) -> String {
    match value.take() {
        Value::String(text) => text,
        other => panic!("expected a string, found {other}"),
    }
}

fn expect_texts(content: &mut Value, facing: Id, out: &mut Vec<Datum>) {
    for item in content.as_array_mut().unwrap() {
        if item["text"] != "" {
            out.push((modality::TEXT, facing, taken_text(&mut item["text"])));
        }
    }
}

fn expect_change(item: &mut Value, words: &[&str], facing: Id, out: &mut Vec<Datum>) {
    let kind = item["type"].clone();
    let mut event = BTreeMap::new();
    for word in words {
        event.insert((*word).to_owned(), item[*word].take());
    }
    let change = &mut item["output_diff"];
    for name in [
        "base_commit_sha",
        "commit_message",
        "files_modified",
        "lines_added",
        "lines_removed",
        "repo_id",
    ] {
        if !change[name].is_null() {
            event.insert(name.to_owned(), change[name].take());
        }
    }
    if !event.is_empty() {
        event.insert("type".to_owned(), kind);
        out.push((
            modality::EVENT,
            facing,
            serde_json::to_string(&event).unwrap(),
        ));
    }
    let file = if change["diff"].is_null() {
        taken_text(&mut change["external_storage_diff"]["file_id"])
    } else {
        taken_text(&mut change["diff"])
    };
    out.push((modality::FILE, facing, file));
}

/// The data of one task in the order the export gives, taken out of `task`.
fn expected(task: &mut Value) -> Vec<Datum> {
    use direction::{IN, OUT};
    let mut out = Vec::new();
    for turn in task["turns"].as_array_mut().unwrap() {
        if turn["role"] == "user" {
            let items = turn["input_items"].as_array_mut().unwrap();
            for item in items.iter_mut() {
                if item["type"] == "prior_conversation" {
                    for said in item["conversation"].as_array_mut().unwrap() {
                        let facing = if said["role"] == "user" { IN } else { OUT };
                        expect_texts(&mut said["content"], facing, &mut out);
                    }
                }
            }
            for item in items.iter_mut() {
                match item["type"].as_str().unwrap() {
                    "message" => expect_texts(&mut item["content"], IN, &mut out),
                    "comment" => {
                        expect_texts(&mut item["content"], IN, &mut out);
                        out.push((modality::EVENT, IN, item["position"].take().to_string()));
                    }
                    "ide_context" => {
                        out.push((modality::TEXT, IN, taken_text(&mut item["context"])))
                    }
                    "pre_apply_patch" => expect_change(item, &[], IN, &mut out),
                    "prior_conversation" => {}
                    other => panic!("input item {other}"),
                }
            }
        } else {
            for item in turn["output_items"].as_array_mut().unwrap() {
                match item["type"].as_str().unwrap() {
                    "message" => {
                        for piece in item["content"].as_array_mut().unwrap() {
                            if piece["content_type"] == "text" {
                                if piece["text"] != "" {
                                    out.push((modality::TEXT, OUT, taken_text(&mut piece["text"])));
                                }
                            } else {
                                out.push((modality::EVENT, OUT, piece.take().to_string()));
                            }
                        }
                    }
                    "pr" => expect_change(item, &["pr_message", "pr_title"], OUT, &mut out),
                    "follow_up_diff" => expect_change(item, &[], OUT, &mut out),
                    "partial_repo_snapshot" => {}
                    other => panic!("output item {other}"),
                }
            }
        }
    }
    out
}

/// Count what is left in the tree, by path. `files` is counted whole.
fn unrendered(value: &Value, path: &str, counts: &mut BTreeMap<String, usize>) {
    match value {
        Value::Null => {}
        Value::Object(members) => {
            for (name, member) in members {
                unrendered(member, &format!("{path}.{name}"), counts);
            }
        }
        Value::Array(elements) if path.ends_with(".files") => {
            *counts.entry(path.to_owned()).or_default() += elements.len();
        }
        Value::Array(elements) => {
            for element in elements {
                unrendered(element, &format!("{path}[]"), counts);
            }
        }
        _ => *counts.entry(path.to_owned()).or_default() += 1,
    }
}

/// Project each task alone and compare it with the second reading.
fn gate(tasks: &Value, counts: &mut BTreeMap<String, usize>) -> usize {
    let mut nodes = 0;
    for (index, task) in tasks.as_array().unwrap().iter().enumerate() {
        let found = project(&Value::Array(vec![task.clone()])).unwrap();
        let mut tree = task.clone();
        let mut expected = expected(&mut tree);
        assert_eq!(found.nodes.len(), expected.len(), "task {index} node count");
        let mut data = found.data.clone();
        if found.summary.stats.missing_predecessors == 0 {
            // One chain: the order is compared too.
            let chain = found
                .nodes
                .last()
                .map(|leaf| blockdag::thread_to(&found.facts, *leaf))
                .unwrap_or_default();
            assert!(chain == found.nodes, "task {index} is not one chain");
        } else {
            data.sort();
            expected.sort();
        }
        assert!(data == expected, "task {index} content or order differs");
        nodes += found.nodes.len();
        unrendered(&tree, "", counts);
    }
    for path in counts.keys() {
        assert!(
            UNRENDERED.contains(&path.as_str()),
            "{path} is neither rendered nor left out by name"
        );
    }
    nodes
}

#[test]
fn every_value_is_rendered_or_left_out_by_name() {
    let mut counts = BTreeMap::new();
    let nodes = gate(&json!([task()]), &mut counts);
    assert_eq!(nodes, 15);
    assert_eq!(counts[".turns[].output_items[].files"], 1);
    assert_eq!(counts[".title"], 1);
    assert_eq!(counts[".turns[].input_items[].content[].text"], 1);
}

#[test]
fn the_gate_notices_a_value_the_importer_does_not_render() {
    // The second reading of a task with one more sentence than was projected.
    let projected = project(&json!([task()])).unwrap();
    let mut longer = task();
    longer["turns"][1]["output_items"][2]["content"]
        .as_array_mut()
        .unwrap()
        .push(text("One more sentence."));
    assert_ne!(projected.data, expected(&mut longer));
}

/// The gate over a real export: `CODEX_CLOUD_EXPORT` names its directory.
/// Counts only; no value is printed.
#[test]
#[ignore = "reads a real export named by CODEX_CLOUD_EXPORT"]
fn every_value_of_a_real_export_is_accounted_for() {
    let root = std::env::var("CODEX_CLOUD_EXPORT").expect("CODEX_CLOUD_EXPORT");
    let mut files: Vec<_> = fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| is_task_file(path))
        .collect();
    files.sort();
    let mut counts = BTreeMap::new();
    let (mut tasks, mut nodes) = (0, 0);
    for file in files {
        let shard: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        tasks += shard.as_array().unwrap().len();
        nodes += gate(&shard, &mut counts);
    }
    println!("{tasks} tasks, {nodes} nodes; not rendered, by path:");
    for (path, count) in counts {
        println!("  {count:>7} {path}");
    }
}

// ---------------------------------------------------------------------------
// The projection's own rules
// ---------------------------------------------------------------------------

#[test]
fn a_prior_conversation_comes_before_the_message() {
    let projected = project(&json!([task()])).unwrap();
    let texts: Vec<_> = projected
        .data
        .iter()
        .filter(|(kind, _, _)| *kind == modality::TEXT)
        .map(|(_, facing, text)| (*facing == direction::OUT, text.as_str()))
        .collect();
    assert_eq!(
        &texts[..4],
        &[
            (false, "Are the corners square?"),
            (true, "They are."),
            (false, "open file: widget.rs"),
            (false, "Please round the widget's corners."),
        ]
    );
}

#[test]
fn ids_and_times_stay_out_of_identity() {
    let here = project(&json!([task()])).unwrap();
    let spelled = serde_json::to_string(&task())
        .unwrap()
        .replace("68a0", "68b1");
    let there = project(&json!([serde_json::from_str::<Value>(&spelled).unwrap()])).unwrap();
    assert_eq!(here.nodes, there.nodes);
    let times = |facts: &TribleSet| -> BTreeSet<_> {
        find!(
            time: Inline<NsTAIInterval>,
            pattern!(facts, [{ schema::block::timestamp: ?time }])
        )
        .map(|time| time.raw)
        .collect()
    };
    assert!(times(&here.facts).is_disjoint(&times(&there.facts)));
}

#[test]
fn the_task_id_annotates_the_leaf_and_the_turn_second_its_nodes() {
    let projected = project(&json!([task()])).unwrap();
    let leaf = *projected.nodes.last().unwrap();
    let located: Vec<Id> = find!(
        node: Id,
        pattern!(&projected.facts, [{
            schema::source_projection::source_namespace: &SOURCE_CODEX_CLOUD,
            schema::source_projection::projects_to: ?node,
            schema::source_projection::source_locator: _?locator
        }])
    )
    .collect();
    assert_eq!(located, vec![leaf]);
    let epoch = Epoch::from_unix_seconds(f64::from(0x68a0_0002u32));
    let expected: Inline<NsTAIInterval> = (epoch, epoch).try_to_inline().unwrap();
    assert!(exists!(pattern!(&projected.facts, [{
        leaf @ schema::block::timestamp: expected
    }])));
}

#[test]
fn a_longer_task_only_appends_nodes() {
    let whole = project(&json!([task()])).unwrap();
    let mut shorter = task();
    shorter["turns"].as_array_mut().unwrap().truncate(2);
    let shorter = project(&json!([shorter])).unwrap();
    assert!(shorter.nodes.len() < whole.nodes.len());
    assert_eq!(shorter.nodes[..], whole.nodes[..shorter.nodes.len()]);
}

#[test]
fn a_turn_whose_predecessor_is_not_held_follows_nothing() {
    let mut orphaned = task();
    orphaned["turns"][1]["previous_turn_id"] = json!(turn_id("usertrn", 0x68a0_0001, 77));
    let projected = project(&json!([orphaned])).unwrap();
    assert_eq!(projected.summary.stats.missing_predecessors, 1);
    let roots = projected
        .nodes
        .iter()
        .filter(|node| {
            !exists!(pattern!(&projected.facts, [{
                **node @ schema::block::previous: _?previous
            }]))
        })
        .count();
    assert_eq!(roots, 2);
}

#[test]
fn the_same_task_twice_is_emitted_once_and_shard_order_does_not_matter() {
    let mut other = task();
    other["turns"][0]["input_items"][1]["content"][0]["text"] = json!("Another request.");
    let one = project(&json!([task(), other])).unwrap();
    let two = project(&json!([other, task(), task()])).unwrap();
    assert_eq!(one.nodes, two.nodes);
    assert_eq!(one.summary.stats, {
        let mut stats = two.summary.stats;
        stats.records_seen = one.summary.stats.records_seen;
        stats.contentless_records = one.summary.stats.contentless_records;
        stats
    });
}

#[test]
fn what_is_left_out_is_counted() {
    let summary = project(&json!([task()])).unwrap().summary;
    assert_eq!(summary.empty_texts, 1);
    assert_eq!(summary.unresolved_files, 2);
    assert_eq!(summary.dropped["partial_repo_snapshot.files"], 1);
    assert_eq!(summary.dropped["task.title"], 1);
    assert_eq!(summary.dropped["assistant_turn.turn_status"], 2);
    assert!(!summary
        .dropped
        .contains_key("assistant_turn.external_pull_request_id"));
}

/// Anything the shapes do not name fails the unit, and nothing is emitted.
#[test]
fn an_unknown_value_fails_the_whole_unit() {
    type Break = (&'static str, fn(&mut Value));
    let breaks: &[Break] = &[
        ("member", |t| t["surprise"] = json!(1)),
        ("member in a turn", |t| t["turns"][1]["surprise"] = json!(1)),
        ("member in a diff", |t| {
            t["turns"][1]["output_items"][0]["output_diff"]["surprise"] = json!(1)
        }),
        ("member in a citation", |t| {
            t["turns"][1]["output_items"][2]["content"][1]["surprise"] = json!(1)
        }),
        ("member in a position", |t| {
            t["turns"][2]["input_items"][1]["position"]["surprise"] = json!(1)
        }),
        ("missing member", |t| {
            t["turns"][1].as_object_mut().unwrap().remove("branch");
        }),
        ("input item type", |t| {
            t["turns"][0]["input_items"][0]["type"] = json!("image")
        }),
        ("output item type", |t| {
            t["turns"][1]["output_items"][0]["type"] = json!("log")
        }),
        ("content type", |t| {
            t["turns"][1]["output_items"][2]["content"][0]["content_type"] = json!("image")
        }),
        ("role", |t| t["turns"][0]["role"] = json!("system")),
        ("prior role", |t| {
            t["turns"][0]["input_items"][3]["conversation"][0]["role"] = json!("tool")
        }),
        ("value where only null is known", |t| {
            t["turns"][0]["custom_instructions"] = json!("be brief")
        }),
        ("diff both inline and external", |t| {
            t["turns"][1]["output_items"][0]["output_diff"]["external_storage_diff"] =
                json!({"file_id": "file_3", "ttl": null})
        }),
        ("diff neither inline nor external", |t| {
            t["turns"][1]["output_items"][0]["output_diff"]["diff"] = Value::Null
        }),
        ("id form", |t| {
            t["turns"][0]["id"] = json!(format!("{TASK_ID}~usertrn_x_1"))
        }),
        ("turn seconds out of order", |t| {
            t["turns"][2]["id"] = json!(turn_id("usertrn", 0x68a0_0000, 2))
        }),
        ("two user turns in a row", |t| {
            t["turns"].as_array_mut().unwrap().remove(1);
        }),
        ("text that is a number", |t| {
            t["turns"][1]["output_items"][2]["content"][0]["text"] = json!(3)
        }),
    ];
    for (name, change) in breaks {
        let mut broken = task();
        change(&mut broken);
        let mut emitted = 0;
        let result = project_bytes(
            "codex.json",
            Bytes::from_source(serde_json::to_vec(&json!([task(), broken])).unwrap()),
            |_| {
                emitted += 1;
                Ok(())
            },
        );
        assert!(result.is_err(), "{name} was accepted");
        assert_eq!(emitted, 0, "{name} emitted a partial unit");
    }
}

#[test]
fn a_repeated_member_fails_the_unit() {
    let raw = serde_json::to_string(&json!([task()])).unwrap();
    let repeated = raw.replacen(
        "\"archived\":true",
        "\"archived\":true,\"archived\":false",
        1,
    );
    assert_ne!(raw, repeated);
    assert!(project_bytes("codex.json", Bytes::from_source(repeated), |_| Ok(())).is_err());
}

#[test]
fn only_task_shards_are_read_from_a_directory() {
    let directory = tempfile::tempdir().unwrap();
    for name in [
        "conversations-000.json",
        "codex-x.json",
        "codex-000.json.part",
    ] {
        fs::write(directory.path().join(name), "not json").unwrap();
    }
    assert!(project_path(directory.path(), |_| Ok(())).is_err());
    for name in ["codex-000.json", "codex-001.json"] {
        fs::write(
            directory.path().join(name),
            serde_json::to_vec(&json!([task()])).unwrap(),
        )
        .unwrap();
    }
    let summary = project_path(directory.path(), |_| Ok(())).unwrap();
    assert_eq!((summary.files_scanned, summary.tasks_seen), (2, 2));
}
