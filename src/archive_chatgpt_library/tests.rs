use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;

use serde_json::{json, Value};
use triblespace::core::metadata;
use triblespace::core::repo::{BlobStoreGet, SnapshotSource};
use triblespace::prelude::blobencodings::{RawBytes, UTF8String};
use triblespace::prelude::inlineencodings::Handle;

use super::*;

const LOOSE: &str = "file_00000000000000000000000000000001";
const PLAIN: &str = "file-0000000000000000000002";
const NAMED: &str = "file_00000000000000000000000000000003";
const ABSENT: &str = "file_00000000000000000000000000000004";
const PAGED: &str = "file_00000000000000000000000000000005";
const THREAD: &str = "00000000-0000-4000-8000-000000000001";

fn record(file_id: &str, name: &str, mime: &str) -> Value {
    json!({
        "content_backing_kind": "sediment",
        "context_scopes_v2": null,
        "created_at": "2026-01-02T03:04:05.000300+00:00",
        "deleted_at": null,
        "deletion_origin": null,
        "deletion_reason": null,
        "etag": "an-etag",
        "file_extension": "png",
        "file_id": file_id,
        "file_name": name,
        "file_name_provenance": "upload",
        "file_upload_time": "2026-01-02T03:04:05.000000+00:00",
        "id": {"id": "a-key", "partition_key": "a-partition"},
        "library_file_category": "image",
        "mime_type": mime,
        "record_creation_time": "2026-01-02T03:04:05.000042+00:00",
        "state": "ready",
        "uploading_account_user_id": "user-x",
        "version_created_at": "2026-01-02T03:04:05.000042+00:00",
        "version_provenance": {
            "action": "create", "actor": "user", "actor_id": "user-x", "reason": null,
        },
    })
}

fn bytes_of(file_id: &str) -> Vec<u8> {
    format!("the bytes of {file_id}").into_bytes()
}

/// One record of every kind the importer tells apart.
fn library() -> Value {
    let mut loose = record(LOOSE, "a generated picture.png", "image/png");
    loose["file_size_bytes"] = json!(bytes_of(LOOSE).len());
    loose["sha256_digest"] = json!(hex::encode(Sha256::digest(bytes_of(LOOSE))));
    loose["image_gen_generation_id"] = json!("s_00000000000000000000000000000001");
    loose["origination_thread_id"] = json!(THREAD);
    loose["origination_message_id"] = json!("a-message-no-export-holds");
    loose["producer_metadata"] = Value::Null;
    loose["thumbnail_sources"] = json!({"small": {"source_id": "x"}});
    let plain = record(PLAIN, "papers.pdf", "application/pdf");
    let mut named = record(NAMED, "uploaded.png", "image/png");
    named["producer_metadata"] = json!({"upload_origin": "conversation"});
    let mut absent = record(ABSENT, "not here.png", "image/png");
    absent["image_gen_generation_id"] = json!("s_00000000000000000000000000000002");
    json!([
        loose,
        plain,
        named,
        absent,
        record(PAGED, "paged.pdf", "application/pdf")
    ])
}

fn conversations() -> Value {
    json!([{"id": THREAD, "mapping": {
        "a": {"id": "a", "parent": null, "message": {
            "id": "ma", "author": {"role": "user"},
            "content": {"content_type": "multimodal_text", "parts": [
                {"content_type": "image_asset_pointer", "asset_pointer": format!("sediment://{NAMED}"), "size_bytes": 3},
                {"content_type": "image_asset_pointer", "asset_pointer": format!("sediment://hash#{PAGED}#p_0.jpg"), "size_bytes": 3},
            ]},
            "metadata": {"citations": [{"url": format!("https://example.invalid/{PLAIN}")}]},
        }},
    }}])
}

fn export(library: &Value) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    let write = |name: &str, bytes: Vec<u8>| fs::write(root.path().join(name), bytes).unwrap();
    write(LIBRARY, serde_json::to_vec(library).unwrap());
    write(
        "conversations-000.json",
        serde_json::to_vec(&conversations()).unwrap(),
    );
    for id in [LOOSE, PLAIN, NAMED, PAGED] {
        write(&format!("{id}.dat"), bytes_of(id));
    }
    root
}

struct Projected {
    summary: ProjectionSummary,
    facts: TribleSet,
    identity: TribleSet,
    union: Fragment,
}

fn project(root: &Path) -> Result<Projected> {
    let mut union = Fragment::empty();
    let mut identity = TribleSet::new();
    let summary = project_path(root, |projected| {
        assert!(projected.identity.nodes().is_empty());
        identity += projected.identity.facts().clone();
        union += projected.fragment;
        Ok(())
    })?;
    let facts = union.facts().clone();
    Ok(Projected {
        summary,
        facts,
        identity,
        union,
    })
}

fn refused(library: Value) -> String {
    let root = export(&library);
    let mut emitted = 0;
    let error = project_path(root.path(), |_| {
        emitted += 1;
        Ok(())
    })
    .expect_err("the unit must fail");
    assert_eq!(emitted, 0, "a refused unit emits nothing");
    format!("{error:#}")
}

// ---------------------------------------------------------------------------
// The gate: a second reading, with serde_json, sharing no code with the
// importer. It predicts the facts of every record, takes them out of what was
// projected and requires nothing to be left on either side.
// ---------------------------------------------------------------------------

/// Every string in the content and the attachments of every message, where a
/// conversation's nodes get their media from, and every string anywhere.
fn spoken(value: &Value, within: bool, media: &mut Vec<String>, all: &mut Vec<String>) {
    match value {
        Value::String(text) => {
            all.push(text.clone());
            if within {
                media.push(text.clone());
            }
        }
        Value::Array(items) => items
            .iter()
            .for_each(|item| spoken(item, within, media, all)),
        Value::Object(members) => members.iter().for_each(|(name, member)| {
            let within = within || name == "content" || name == "attachments";
            spoken(member, within, media, all)
        }),
        _ => {}
    }
}

/// What a library record became, by the gate's own reading.
fn fate(record: &Value, shipped: &HashMap<String, std::path::PathBuf>, said: &str) -> &'static str {
    let id = record["file_id"].as_str().unwrap();
    match (shipped.contains_key(id), said.contains(id)) {
        (false, _) => "not shipped",
        (true, true) => "named by a conversation",
        (true, false) => "observed",
    }
}

const KEPT: [&str; 11] = [
    "file_id",
    "file_name",
    "file_size_bytes",
    "image_gen_generation_id",
    "mime_type",
    "origination_message_id",
    "origination_thread_id",
    "producer_metadata",
    "record_creation_time",
    "sha256_digest",
    "version_provenance",
];

/// Every record of the export accounted for; counts by fate and, for the
/// observed ones, by member not rendered.
fn gate(root: &Path) -> BTreeMap<String, usize> {
    let library: Value = serde_json::from_slice(&fs::read(root.join(LIBRARY)).unwrap()).unwrap();
    let mut shipped = HashMap::new();
    let (mut media, mut all) = (Vec::new(), Vec::new());
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_str().unwrap().to_owned();
        if name.starts_with("conversations-") {
            let shard: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            spoken(&shard, false, &mut media, &mut all);
        } else if let Some(id) = name.strip_suffix(".dat") {
            shipped.insert(id.to_owned(), path);
        }
    }
    let said = media.join("\n");
    let mentioned = all.join("\n");

    let projected = project(root).unwrap();
    let mut blobs = projected.union.blobs().clone();
    let reader = blobs.snapshot().unwrap();
    let text = |handle: Inline<Handle<UTF8String>>| -> String {
        let view: View<str> = reader.get(handle).unwrap();
        view.as_ref().to_owned()
    };
    let one = |entity: Id, attribute: &Attribute<Handle<UTF8String>>| -> Option<String> {
        let found: Vec<_> = find!(
            value: Inline<Handle<UTF8String>>,
            pattern!(&projected.facts, [{ entity @ attribute: ?value }])
        )
        .collect();
        assert!(found.len() <= 1);
        found.first().map(|handle| text(*handle))
    };
    let mut held: BTreeMap<[u8; 32], Id> = find!(
        (file: Id, bytes: Inline<Handle<RawBytes>>),
        pattern!(&projected.facts, [{
            ?file @ metadata::tag: &schema::library_file::KIND,
            schema::content_fact::blob: ?bytes
        }])
    )
    .map(|(file, bytes)| (bytes.raw, file))
    .collect();

    let mut counts = BTreeMap::new();
    for record in library.as_array().unwrap() {
        let fate = fate(record, &shipped, &said);
        *counts.entry(format!("record: {fate}")).or_default() += 1;
        if fate != "observed" {
            continue;
        }
        let id = record["file_id"].as_str().unwrap();
        if mentioned.contains(id) {
            *counts
                .entry("observed, mentioned outside content and attachments".to_owned())
                .or_default() += 1;
        }
        let bytes = fs::read(&shipped[id]).unwrap();
        let file = held
            .remove(blake3::hash(&bytes).as_bytes())
            .expect("an observed record has its observation");
        let stored: Bytes = reader
            .get::<Bytes, RawBytes>(Inline::new(*blake3::hash(&bytes).as_bytes()))
            .unwrap();
        assert!(stored.as_ref() == bytes.as_slice());
        let stated = |name: &str| record.get(name).and_then(Value::as_str).map(str::to_owned);
        assert!(one(file, &metadata::name) == stated("file_name"));
        assert!(one(file, &schema::library_file::generation) == stated("image_gen_generation_id"));
        assert!(
            one(file, &schema::library_file::origin_message) == stated("origination_message_id")
        );
        assert!(
            one(file, &schema::source_projection::source_locator)
                == stated("origination_thread_id").map(|id| format!("conversation/{id}"))
        );
        let pointer = one(file, &schema::content_fact::asset_pointer).unwrap();
        assert!(pointer.split_once("://").unwrap().1 == id);
        let media: Vec<Id> = find!(
            media: Id,
            pattern!(&projected.facts, [{ file @ schema::content_fact::media_type: ?media }])
        )
        .collect();
        assert_eq!(media.len(), 1);
        assert!(one(media[0], &metadata::name) == stated("mime_type"));
        let times: Vec<(i128, i128)> = find!(
            time: (i128, i128),
            pattern!(&projected.facts, [{ file @ schema::source_projection::source_timestamp: ?time }])
        )
        .collect();
        assert_eq!(times.len(), 1);
        let time = humantime::parse_rfc3339(
            &stated("record_creation_time")
                .unwrap()
                .replace("+00:00", "Z"),
        )
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
        let expected = Epoch::from_unix_duration(hifitime::Duration::from_total_nanoseconds(
            time.as_nanos() as i128,
        ));
        assert_eq!(
            times[0],
            (
                expected.to_tai_duration().total_nanoseconds(),
                expected.to_tai_duration().total_nanoseconds()
            )
        );
        // Nothing else is said of the file: no direction, above all.
        let attributes = projected
            .facts
            .iter()
            .filter(|fact| *fact.e() == file)
            .count();
        let optional = [
            "image_gen_generation_id",
            "origination_message_id",
            "origination_thread_id",
        ]
        .iter()
        .filter(|name| stated(name).is_some())
        .count();
        assert_eq!(attributes, 3 + 5 + optional);
        for (name, value) in record.as_object().unwrap() {
            if !KEPT.contains(&name.as_str()) && !value.is_null() {
                *counts.entry(format!("not rendered: {name}")).or_default() += 1;
            }
        }
    }
    assert!(held.is_empty(), "an observation no record accounts for");
    assert!(!exists!(pattern!(&projected.facts, [{
        schema::content_fact::direction: _?direction
    }])));
    assert!(!exists!(pattern!(&projected.facts, [{
        schema::block::holds: _?fact
    }])));
    let count = |fate: &str| counts.get(&format!("record: {fate}")).copied().unwrap_or(0);
    assert_eq!(
        projected.summary.records_seen,
        library.as_array().unwrap().len()
    );
    assert_eq!(projected.summary.observed, count("observed"));
    assert_eq!(
        projected.summary.named_by_conversation,
        count("named by a conversation")
    );
    assert_eq!(projected.summary.not_shipped, count("not shipped"));
    counts
}

#[test]
fn every_record_is_accounted_for() {
    let root = export(&library());
    let counts = gate(root.path());
    assert_eq!(counts["record: observed"], 2);
    assert_eq!(counts["record: named by a conversation"], 2);
    assert_eq!(counts["record: not shipped"], 1);
    assert_eq!(counts["not rendered: etag"], 2);
    // A mention no node is made of leaves the file to the library.
    assert_eq!(
        counts["observed, mentioned outside content and attachments"],
        1
    );
}

/// The gate over a real export: `CHATGPT_EXPORT` names its directory. Counts
/// only; no value is printed.
#[test]
#[ignore = "reads a real export named by CHATGPT_EXPORT"]
fn every_record_of_a_real_export_is_accounted_for() {
    let root = std::env::var("CHATGPT_EXPORT").expect("CHATGPT_EXPORT");
    for (path, count) in gate(Path::new(&root)) {
        println!("  {count:>7} {path}");
    }
}

// ---------------------------------------------------------------------------
// The projection's own rules
// ---------------------------------------------------------------------------

#[test]
fn identity_is_the_source_and_the_bytes() {
    let here = project(export(&library()).path()).unwrap();
    let mut other = library();
    for record in other.as_array_mut().unwrap() {
        record["file_name"] = json!("another name");
        record["record_creation_time"] = json!("2027-01-02T03:04:05.000042+00:00");
        record["origination_thread_id"] = json!("another-thread");
        record["image_gen_generation_id"] = json!("another-generation");
        record["mime_type"] = json!("image/webp");
    }
    other.as_array_mut().unwrap().reverse();
    let there = project(export(&other).path()).unwrap();
    assert_eq!(here.identity, there.identity);
    assert_ne!(here.facts, there.facts);
}

#[test]
fn a_file_once_named_by_a_message_meets_its_observation() {
    let root = export(&library());
    let library = project(root.path()).unwrap();
    let mut says = conversations();
    says[0]["mapping"]["a"]["message"]["content"]["parts"]
        .as_array_mut()
        .unwrap()
        .push(json!({
            "content_type": "image_asset_pointer",
            "asset_pointer": format!("sediment://{LOOSE}"),
        }));
    fs::write(
        root.path().join("conversations-000.json"),
        serde_json::to_vec(&says).unwrap(),
    )
    .unwrap();
    let mut facts = library.facts.clone();
    archive_chatgpt::project_path(root.path(), |projected| {
        facts += projected.fragment.facts().clone();
        Ok(())
    })
    .unwrap();
    // By the bytes the message's pointer resolves to, and by the pointer.
    assert_eq!(
        find!(
            file: Id,
            pattern!(&facts, [
                { ?file @ metadata::tag: &schema::library_file::KIND,
                    schema::content_fact::blob: _?bytes,
                    schema::content_fact::asset_pointer: _?pointer },
                { _?node @ schema::block::holds: _?fact },
                { _?fact @ schema::content_fact::resolved_to: _?bytes,
                    schema::content_fact::asset_pointer: _?pointer },
            ])
        )
        .collect::<BTreeSet<_>>()
        .len(),
        1
    );
    // By the conversation the library names: the same locator as the leaf.
    assert!(exists!(pattern!(&facts, [
        { _?file @ metadata::tag: &schema::library_file::KIND,
            schema::source_projection::source_locator: _?locator },
        { _?receipt @ schema::source_projection::projects_to: _?leaf,
            schema::source_projection::source_locator: _?locator },
    ])));
    // The record is the conversation's now; the observation stays true.
    let now = project(root.path()).unwrap();
    assert_eq!(now.summary.observed, 1);
    assert_eq!(now.summary.named_by_conversation, 3);
}

#[test]
fn an_unknown_missing_or_unexpected_member_fails_the_unit() {
    let plant = |edit: &dyn Fn(&mut Value)| {
        let mut planted = library();
        edit(&mut planted[0]);
        refused(planted)
    };
    assert!(plant(&|r| r["something_new"] = json!(1)).contains("unknown member"));
    assert!(plant(&|r| {
        r.as_object_mut().unwrap().remove("mime_type");
    })
    .contains("no member"));
    assert!(plant(&|r| r["deleted_at"] = json!("then")).contains("only null is understood"));
    assert!(plant(&|r| r["version_provenance"]["actor"] = json!("assistant")).contains("actor"));
    assert!(plant(&|r| r["version_provenance"]["action"] = json!("edit")).contains("action"));
    assert!(plant(&|r| r["version_provenance"]["reason"] = json!("why")).contains("reason"));
    assert!(plant(&|r| r["version_provenance"]["more"] = json!(1)).contains("unknown member"));
    assert!(
        plant(&|r| r["producer_metadata"] = json!({"upload_origin": "unknown"}))
            .contains("upload origin")
    );
    assert!(plant(&|r| r["producer_metadata"] = json!({"other": "x"})).contains("unknown member"));
    assert!(plant(&|r| r["record_creation_time"] = json!("yesterday")).contains("not a time"));
    assert!(plant(&|r| r["file_size_bytes"] = json!(1)).contains("not the size"));
    assert!(plant(&|r| r["file_size_bytes"] = json!(-1)).contains("not a size"));
    assert!(plant(&|r| r["sha256_digest"] = json!("00")).contains("SHA-256"));
}

#[test]
fn a_fault_late_in_the_library_emits_nothing_of_it() {
    let mut planted = library();
    planted[4]["something_new"] = json!(1);
    assert!(refused(planted).contains("library record 4"));
}

#[test]
fn an_export_without_conversations_or_with_two_files_of_one_id_fails() {
    let root = export(&library());
    fs::copy(
        root.path().join(format!("{LOOSE}.dat")),
        root.path().join(format!("{LOOSE}.png")),
    )
    .unwrap();
    assert!(format!("{:#}", project(root.path()).err().unwrap()).contains("several files"));
    let root = export(&library());
    fs::remove_file(root.path().join("conversations-000.json")).unwrap();
    assert!(format!("{:#}", project(root.path()).err().unwrap()).contains("no conversations"));
}

#[test]
fn a_media_type_the_library_leaves_empty_is_not_stated() {
    let mut untyped = library();
    untyped[1]["mime_type"] = json!("");
    let projected = project(export(&untyped).path()).unwrap();
    assert_eq!(projected.summary.media_types["untyped"], 1);
    assert_eq!(projected.summary.observed, 2);
}
