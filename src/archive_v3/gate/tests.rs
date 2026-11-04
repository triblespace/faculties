//! Gate tests, on synthetic data only.
//!
//! Three groups: the walker (B) on its own; the gate against hand-built v3
//! renderings, which must pass when right and report each kind of fault when
//! wrong; and the two witnesses that must fail on main's importers.

use std::str::FromStr;

use anybytes::Bytes;
use base64::Engine;
use hifitime::{Duration, Epoch};
use triblespace::core::blob::MemoryBlobStore;
use triblespace::core::blob::MemoryBlobStoreSnapshot;
use triblespace::core::import::json_tree::JsonTreeImporter;
use triblespace::core::repo::SnapshotSource;
use triblespace::prelude::inlineencodings::NsTAIInterval;

use super::walk::{probe, walk};
use super::*;
use crate::archive_v3::fields::{
    chatgpt, claude_code, Class, Cond, Decode, DropReason, Entry, FieldTable, Mirror, Order,
    Structure,
};
use crate::archive_v3::{
    annotate_node, annotate_witness, fact, gloss, node, unexported, witness, FactCore, MediaType,
    NodeAnnotations, WitnessAnnotations,
};
use crate::schemas::archive_v3 as v3;
use crate::schemas::blockdag::source_projection::{SOURCE_CHATGPT, SOURCE_CLAUDE_CODE};

// ---------------------------------------------------------------------------
// The walker alone.

const TEST_ENTRIES: &[Entry] = &[
    Entry::new("{say}.type", Class::Structure(Structure::Discriminator)),
    Entry::new("{say}.id", Class::Structure(Structure::Order)),
    Entry::new("{say}.parent", Class::Structure(Structure::Order)),
    Entry::fact("{say}.text", "text", "out", "payload", Decode::Text),
    Entry::fact("{say}.args", "tool-call", "out", "structure", Decode::Json),
    Entry::fact("{say}.bytes", "image", "in", "blob", Decode::Base64),
    Entry::fact("{say}.count", "text", "out", "payload", Decode::Number),
    Entry::new(
        "{say}.copy",
        Class::Drop(DropReason::Mirror(Mirror::Sibling("text"))),
    ),
    Entry::new("{say}.unused", Class::Drop(DropReason::Empty)),
    Entry::new(
        "{say}.mood",
        Class::Structure(Structure::Vocabulary(&["calm"])),
    ),
    Entry::new("{say}.puzzle", Class::Open("not yet verified")),
    Entry::new("{say}.ref", Class::Correlator),
    Entry::fact(
        "{say}.how",
        "text",
        "out",
        "register",
        Decode::Term(&[("aside", "commentary")]),
    ),
    Entry::new(
        "{say}.who",
        Class::Annotation(&[("raw_role", Decode::Text)]),
    )
    .when(&[Cond::Is("who", "me")]),
    Entry::new("{say}.parts[]", Class::Drop(DropReason::Machinery("test"))),
];

const TEST: FieldTable = FieldTable {
    format: "test",
    jsonl: true,
    discriminators: &["type"],
    id_keyed: &[],
    record: "",
    order: Order::Parents {
        id: "id",
        parent: "parent",
    },
    entries: TEST_ENTRIES,
};

fn kinds(walked: &walk::Walked) -> Vec<(String, Kind)> {
    walked
        .rows
        .iter()
        .map(|(path, kind, _)| (path.clone(), *kind))
        .collect()
}

#[test]
fn walker_decodes_values_exactly_and_ignores_member_order() {
    let a = walk(
        &TEST,
        r#"{"type":"say","id":"1","text":"caf\u00e9 \ud83d\ude00\n","args":{"b":[1,2.50,{}],"a":null,"c":[]},"count":1.0}"#
            .as_bytes(),
    );
    let b = walk(
        &TEST,
        r#"{ "count" : 1.0 , "args" : { "c" : [ ] , "a" : null , "b" : [ 1 , 2.50 , { } ] } , "text" : "café 😀\n" , "id" : "1" , "type" : "say" }"#
            .as_bytes(),
    );
    assert!(a.rows.is_empty(), "{:?}", a.rows);
    assert_eq!(
        a.tokens.keys().collect::<Vec<_>>(),
        b.tokens.keys().collect::<Vec<_>>()
    );
    let values: Vec<&Value> = a.tokens.keys().map(|token| &token.value).collect();
    assert!(values.contains(&&Value::Text("café 😀\n".to_owned())));
    assert!(
        values.contains(&&Value::Number("1.0".to_owned())),
        "number tokens stay exact"
    );
    for leaf in [
        Value::Json("/a".to_owned(), JsonLeaf::Null),
        Value::Json("/b/0".to_owned(), JsonLeaf::Number("1".to_owned())),
        Value::Json("/b/1".to_owned(), JsonLeaf::Number("2.50".to_owned())),
        Value::Json("/b/2".to_owned(), JsonLeaf::EmptyObject),
        Value::Json("/c".to_owned(), JsonLeaf::EmptyArray),
    ] {
        assert!(values.contains(&&leaf), "{leaf:?}");
    }
}

#[test]
fn walker_hashes_base64_and_leaves_out_correlators_and_drops() {
    let walked = walk(
        &TEST,
        br#"{"type":"say","id":"1","bytes":"AAEC","ref":"toolu_x","copy":"hi","text":"hi","unused":"","parts":[1,2]}"#,
    );
    assert!(walked.rows.is_empty(), "{:?}", walked.rows);
    let hash = blake3::hash(&[0u8, 1, 2]).to_hex().to_string();
    let values: Vec<&Value> = walked.tokens.keys().map(|token| &token.value).collect();
    assert_eq!(
        values,
        vec![&Value::Bytes(hash), &Value::Text("hi".to_owned())]
    );
}

#[test]
fn walker_fails_what_the_table_does_not_name() {
    let rows = |line: &[u8]| kinds(&walk(&TEST, line));
    assert_eq!(
        rows(br#"{"type":"say","id":"1","novel":1}"#),
        vec![("{say}.novel".to_owned(), Kind::Unknown)]
    );
    assert_eq!(
        rows(br#"{"type":"shout","id":"1"}"#),
        vec![
            ("{shout}.id".to_owned(), Kind::Unknown),
            ("{shout}.type".to_owned(), Kind::Unknown)
        ],
        "an unknown discriminator is an unknown path"
    );
    assert_eq!(
        rows(br#"{"type":"say","mood":"stormy"}"#),
        vec![("{say}.mood".to_owned(), Kind::UnknownValue)]
    );
    assert_eq!(
        rows(br#"{"type":"say","how":"shout"}"#),
        vec![("{say}.how".to_owned(), Kind::UnknownValue)]
    );
    assert_eq!(
        rows(br#"{"type":"say","copy":"hi","text":"ho"}"#),
        vec![("{say}.copy".to_owned(), Kind::Mirror)]
    );
    assert_eq!(
        rows(br#"{"type":"say","unused":"x"}"#),
        vec![("{say}.unused".to_owned(), Kind::Empty)]
    );
    assert_eq!(
        rows(br#"{"type":"say","puzzle":true}"#),
        vec![("{say}.puzzle".to_owned(), Kind::Open)]
    );
    assert_eq!(
        rows(br#"{"type":"say","who":"you"}"#),
        vec![("{say}.who".to_owned(), Kind::Unknown)],
        "no entry applies under these conditions"
    );
    assert_eq!(
        rows(br#"{"type":"say","bytes":"not base64!"}"#),
        vec![("{say}.bytes".to_owned(), Kind::Decode)]
    );
    for bad in [
        &br#"{"type":"say","id":"1","id":"2"}"#[..],
        br#"{"type":"say","text":"\ud800"}"#,
        br#"{"type":"say","count":01}"#,
        br#"{"type":"say"} trailing"#,
    ] {
        assert_eq!(rows(bad), vec![("<document>".to_owned(), Kind::Decode)]);
    }
}

#[test]
fn walker_normalizes_ids_and_checks_children_mirrors() {
    let walked = walk(&chatgpt::TABLE, CHATGPT.as_bytes());
    assert!(walked.rows.is_empty(), "{:?}", walked.rows);
    assert!(walked
        .tokens
        .values()
        .flatten()
        .all(|origin| !origin.path.contains("usr") && !origin.path.contains("call")));
    assert!(walked
        .tokens
        .values()
        .flatten()
        .any(|origin| origin.path == RECIPIENT));

    let wrong = CHATGPT.replace(r#""children": ["usr"]"#, r#""children": ["tool"]"#);
    let walked = walk(&chatgpt::TABLE, wrong.as_bytes());
    assert_eq!(
        kinds(&walked),
        vec![("[].mapping.<id>.children".to_owned(), Kind::Mirror)]
    );
}

#[test]
fn walker_canonicalizes_media_types_and_times_exactly() {
    for (spelled, canonical) in [
        ("image/png", Some("image/png")),
        ("IMAGE/PNG", Some("image/png")),
        (
            "Audio/L16; rate=24000; channels=\"1\"",
            Some("audio/l16;channels=1;rate=24000"),
        ),
        ("text/plain; a=\"x y\"", Some("text/plain;a=\"x y\"")),
        ("text/plain; a=1; A=2", None),
        ("image", None),
        ("", None),
    ] {
        assert_eq!(
            probe::canonical_media_type(spelled).as_deref(),
            canonical,
            "{spelled:?}"
        );
    }
    assert_eq!(
        probe::exact_nanoseconds("1772382850"),
        Some(1772382850_000_000_000)
    );
    assert_eq!(
        probe::exact_nanoseconds("1772382841.5"),
        Some(1772382841_500_000_000)
    );
    assert_eq!(probe::exact_nanoseconds("1.5e3"), Some(1500_000_000_000));
    assert_eq!(probe::exact_nanoseconds("0.0000000001"), None);
}

#[test]
fn seed_tables_name_only_v3_terms() {
    let modalities: Vec<&str> = v3::fact::modality::SPECS.iter().map(|(_, n)| *n).collect();
    let directions: Vec<&str> = v3::fact::direction::SPECS.iter().map(|(_, n)| *n).collect();
    let fact_attributes = [
        "payload",
        "blob",
        "structure",
        "tool_name",
        "tool_namespace",
        "status",
        "exit_code",
        "media_type",
        "source_uri",
        "title",
        "sense",
        "detail",
        "register",
        "form",
    ];
    let mut annotations = vec![
        "raw_model".to_owned(),
        "raw_role".to_owned(),
        "raw_author".to_owned(),
        "generated_by".to_owned(),
        "source_locator".to_owned(),
        "timestamp".to_owned(),
        "pointer".to_owned(),
        "claimed_size".to_owned(),
        "claimed_media_type".to_owned(),
        "claimed_title".to_owned(),
        "compaction_summary".to_owned(),
        "ends".to_owned(),
    ];
    annotations.extend(
        v3::gloss::category::SPECS
            .iter()
            .map(|(_, name)| format!("gloss:{name}")),
    );
    let terms_of = |attribute: &str| -> Vec<&str> {
        match attribute {
            "status" => v3::fact::status::SPECS.iter().map(|(_, n)| *n).collect(),
            "detail" => v3::fact::detail::SPECS.iter().map(|(_, n)| *n).collect(),
            "register" => v3::fact::register::SPECS.iter().map(|(_, n)| *n).collect(),
            "form" => v3::fact::form::SPECS.iter().map(|(_, n)| *n).collect(),
            "ends" => v3::node::ends::SPECS.iter().map(|(_, n, _)| *n).collect(),
            _ => Vec::new(),
        }
    };
    for table in [&claude_code::TABLE, &chatgpt::TABLE, &TEST] {
        for entry in table.entries {
            let check_term = |attribute: &str, decode: &Decode| {
                if let Decode::Term(map) = decode {
                    let known = terms_of(attribute);
                    for (_, term) in *map {
                        assert!(known.contains(term), "{}: {term}", entry.path);
                    }
                }
            };
            match &entry.class {
                Class::Fact {
                    modality,
                    direction,
                    attribute,
                    decode,
                } => {
                    assert!(modalities.contains(modality), "{}", entry.path);
                    assert!(directions.contains(direction), "{}", entry.path);
                    assert!(fact_attributes.contains(attribute), "{}", entry.path);
                    check_term(attribute, decode);
                }
                Class::Annotation(list) => {
                    for (attribute, decode) in *list {
                        assert!(
                            annotations.iter().any(|a| a == attribute),
                            "{}: {attribute}",
                            entry.path
                        );
                        check_term(attribute, decode);
                    }
                }
                _ => {}
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Synthetic sources.

const RESULT_TEXT: &str = r#"{"type":"text","text":"Found one tool."}"#;
const RESULT_IMAGE: &str =
    r#"{"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw0KGgo="}}"#;
const RESULT_REFERENCE: &str = r#"{"type":"tool_reference","tool_name":"WebSearch"}"#;
const RESULT_DOCUMENT: &str = r#"{"type":"document","source":{"type":"base64","media_type":"application/pdf","data":"JVBERi0xLjQK"}}"#;

/// A synthetic Claude Code transcript whose one tool result holds `items`.
fn claude_code_source(items: &[&str]) -> String {
    let items = items.join(",");
    [
        r#"{"type":"user","sessionId":"s-1","uuid":"u1","parentUuid":null,"timestamp":"2026-03-01T10:00:00.000Z","cwd":"/synthetic","version":"0.0.0","message":{"role":"user","content":"Which tools can search?"}}"#.to_owned(),
        r#"{"type":"assistant","sessionId":"s-1","uuid":"a1","parentUuid":"u1","timestamp":"2026-03-01T10:00:01.000Z","requestId":"req_1","message":{"id":"msg_1","type":"message","role":"assistant","model":"synthetic-model","content":[{"type":"tool_use","id":"toolu_1","name":"ToolSearch","input":{"query":"select:WebSearch","max_results":1}}],"stop_reason":"tool_use","stop_sequence":null,"usage":{"input_tokens":1}}}"#.to_owned(),
        format!(r#"{{"type":"user","sessionId":"s-1","uuid":"u2","parentUuid":"a1","timestamp":"2026-03-01T10:00:02.000Z","sourceToolAssistantUUID":"a1","toolUseResult":{{"type":"text","preview":"synthetic"}},"message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"toolu_1","is_error":false,"content":[{items}]}}]}}}}"#),
        r#"{"type":"assistant","sessionId":"s-1","uuid":"a2","parentUuid":"u2","timestamp":"2026-03-01T10:00:03.000Z","requestId":"req_2","message":{"id":"msg_2","type":"message","role":"assistant","model":"synthetic-model","content":[{"type":"text","text":"WebSearch can."}],"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":2}}}"#.to_owned(),
        r#"{"type":"user","sessionId":"s-1","uuid":"u3","parentUuid":"a2","timestamp":"2026-03-01T10:00:04.000Z","message":{"role":"user","content":"Thanks."}}"#.to_owned(),
    ]
    .join("\n")
        + "\n"
}

const CHATGPT: &str = r#"[{
  "id": "conv-1",
  "title": "Synthetic weather",
  "mapping": {
    "root": {"id": "root", "parent": null, "children": ["sys"], "message": null},
    "sys": {"id": "sys", "parent": "root", "children": ["usr"], "message": {"id": "sys",
      "author": {"role": "system", "name": null, "metadata": {}}, "create_time": null,
      "content": {"content_type": "text", "parts": ["Be brief."]}, "recipient": "all",
      "metadata": {"is_visually_hidden_from_conversation": true}}},
    "usr": {"id": "usr", "parent": "sys", "children": ["call"], "message": {"id": "usr",
      "author": {"role": "user", "name": null, "metadata": {}}, "create_time": 1772382841,
      "content": {"content_type": "text", "parts": ["Weather in Bremen?"]}, "recipient": "all"}},
    "call": {"id": "call", "parent": "usr", "children": ["tool"], "message": {"id": "call",
      "author": {"role": "assistant", "name": null, "metadata": {}}, "create_time": 1772382842,
      "content": {"content_type": "code", "text": "search(\"Bremen weather\")"}, "recipient": "web"}},
    "tool": {"id": "tool", "parent": "call", "children": ["answer"], "message": {"id": "tool",
      "author": {"role": "tool", "name": null, "metadata": {}}, "create_time": 1772382843,
      "content": {"content_type": "text", "parts": ["12 degrees, rain"]}, "recipient": "all"}},
    "answer": {"id": "answer", "parent": "tool", "children": [], "message": {"id": "answer",
      "author": {"role": "assistant", "name": null, "metadata": {}}, "create_time": 1772382850,
      "content": {"content_type": "text", "parts": ["Rainy, 12 degrees."]}, "recipient": "all"}}
  }
}]"#;

const RESULT_PREFIX: &str = "{user}.message.content[]{tool_result}.content[]";
const RECIPIENT: &str = "[].mapping.<id>.message.recipient";

fn at(tail: &str) -> String {
    format!("{RESULT_PREFIX}{tail}")
}

// ---------------------------------------------------------------------------
// Hand-built v3 renderings.

struct Chain {
    fragment: Fragment,
    nodes: Vec<Id>,
    namespace: Id,
}

#[derive(Default)]
struct Step {
    fact: FactCore,
    ends: Vec<Id>,
    witness: WitnessAnnotations,
}

impl Chain {
    fn new(namespace: Id) -> Self {
        Chain {
            fragment: Fragment::empty(),
            nodes: Vec::new(),
            namespace,
        }
    }

    fn push(&mut self, step: Step) -> Id {
        let held = fact(step.fact).unwrap();
        let built = node(self.nodes.last().copied(), held).unwrap();
        let id = built.root().unwrap();
        self.fragment += unexported(built);
        self.fragment += annotate_node(
            id,
            &NodeAnnotations {
                ends: step.ends,
                ..NodeAnnotations::default()
            },
        );
        let witnessed = witness(self.namespace, id).unwrap();
        let witness_id = witnessed.root().unwrap();
        self.fragment += unexported(witnessed);
        self.fragment += annotate_witness(witness_id, step.witness).unwrap();
        self.nodes.push(id);
        id
    }

    fn check(mut self, table: &FieldTable, source: &str) -> Report {
        let blobs = reader(&mut self.fragment);
        check(table, source.as_bytes(), self.fragment.facts(), &blobs)
    }
}

fn reader(fragment: &mut Fragment) -> MemoryBlobStoreSnapshot {
    fragment.blobs_mut().snapshot().unwrap()
}

fn tree(json: &str) -> Fragment {
    let mut store = MemoryBlobStore::new();
    let mut tree = JsonTreeImporter::new(&mut store, None)
        .import_str(json)
        .unwrap();
    tree.blobs_mut().union(store);
    tree
}

fn iso(time: &str) -> Inline<NsTAIInterval> {
    let epoch = Epoch::from_str(time).unwrap();
    (epoch, epoch).try_to_inline().unwrap()
}

fn unix(seconds: i128) -> Inline<NsTAIInterval> {
    let epoch =
        Epoch::from_unix_duration(Duration::from_total_nanoseconds(seconds * 1_000_000_000));
    (epoch, epoch).try_to_inline().unwrap()
}

fn said(modality: Id, direction: Id, text: &str) -> FactCore {
    FactCore {
        modality: Some(modality),
        direction: Some(direction),
        payload: Some(text.to_owned()),
        ..FactCore::default()
    }
}

fn by(role: &str, time: Inline<NsTAIInterval>) -> WitnessAnnotations {
    let model = (role == "assistant").then(|| "synthetic-model".to_owned());
    WitnessAnnotations {
        generated_by: model.iter().cloned().collect(),
        raw_model: model,
        raw_role: Some(role.to_owned()),
        timestamp: vec![time],
        ..WitnessAnnotations::default()
    }
}

/// How a hand-built rendering departs from the right one.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fault {
    None,
    DropImage,
    Retext,
    Swap,
    WrongDirection,
    Extra,
}

/// What a v3 Claude Code importer must make of [`claude_code_source`],
/// with `fault` applied.
fn claude_code_v3(witnessed_items: bool, fault: Fault) -> Chain {
    use v3::fact::{direction::*, modality::*};
    let mut chain = Chain::new(SOURCE_CLAUDE_CODE);
    chain.push(Step {
        fact: said(TEXT, IN, "Which tools can search?"),
        witness: by("user", iso("2026-03-01T10:00:00.000Z")),
        ..Step::default()
    });
    chain.push(Step {
        fact: FactCore {
            modality: Some(TOOL_CALL),
            direction: Some(OUT),
            tool_name: Some("ToolSearch".to_owned()),
            structure: Some(tree(r#"{"query":"select:WebSearch","max_results":1}"#)),
            ..FactCore::default()
        },
        ends: vec![v3::node::ends::TOOL_USE],
        witness: by("assistant", iso("2026-03-01T10:00:01.000Z")),
    });
    let in_result = || by("user", iso("2026-03-01T10:00:02.000Z"));
    chain.push(Step {
        fact: FactCore {
            modality: Some(TOOL_RESULT),
            direction: Some(IN),
            status: Some(v3::fact::status::OK),
            ..FactCore::default()
        },
        witness: in_result(),
        ..Step::default()
    });
    let text = Step {
        fact: said(TEXT, IN, "Found one tool."),
        witness: in_result(),
        ..Step::default()
    };
    let image = Step {
        fact: FactCore {
            modality: Some(IMAGE),
            direction: Some(IN),
            blob: Some(Bytes::from(
                base64::engine::general_purpose::STANDARD
                    .decode("iVBORw0KGgo=")
                    .unwrap(),
            )),
            media_type: Some(MediaType::parse("image/png").unwrap()),
            ..FactCore::default()
        },
        witness: in_result(),
        ..Step::default()
    };
    match fault {
        Fault::DropImage => {
            chain.push(text);
        }
        Fault::Swap => {
            chain.push(image);
            chain.push(text);
        }
        _ => {
            chain.push(text);
            chain.push(image);
        }
    }
    if witnessed_items {
        chain.push(Step {
            fact: FactCore {
                modality: Some(TOOL_DEFINITION),
                direction: Some(IN),
                tool_name: Some("WebSearch".to_owned()),
                ..FactCore::default()
            },
            witness: in_result(),
            ..Step::default()
        });
        chain.push(Step {
            fact: FactCore {
                modality: Some(FILE),
                direction: Some(IN),
                blob: Some(Bytes::from(
                    base64::engine::general_purpose::STANDARD
                        .decode("JVBERi0xLjQK")
                        .unwrap(),
                )),
                media_type: Some(MediaType::parse("application/pdf").unwrap()),
                ..FactCore::default()
            },
            witness: in_result(),
            ..Step::default()
        });
    }
    let answer = if fault == Fault::Retext {
        "WebSearch can!"
    } else {
        "WebSearch can."
    };
    chain.push(Step {
        fact: said(TEXT, OUT, answer),
        ends: vec![v3::node::ends::END_TURN],
        witness: by("assistant", iso("2026-03-01T10:00:03.000Z")),
    });
    if fault == Fault::Extra {
        chain.push(Step {
            fact: said(TEXT, IN, "A line no source holds."),
            witness: by("user", iso("2026-03-01T10:00:04.000Z")),
            ..Step::default()
        });
    }
    let last_direction = if fault == Fault::WrongDirection {
        OUT
    } else {
        IN
    };
    let mut leaf = by("user", iso("2026-03-01T10:00:04.000Z"));
    leaf.source_locator = Some("s-1".to_owned());
    chain.push(Step {
        fact: said(TEXT, last_direction, "Thanks."),
        witness: leaf,
        ..Step::default()
    });
    chain
}

#[test]
fn the_gate_passes_a_right_rendering_and_names_each_fault() {
    let control = claude_code_source(&[RESULT_TEXT, RESULT_IMAGE]);
    let report = claude_code_v3(false, Fault::None).check(&claude_code::TABLE, &control);
    assert!(report.passed(), "{report}");

    let report = claude_code_v3(false, Fault::DropImage).check(&claude_code::TABLE, &control);
    assert_eq!(
        report.count(&at("{image}.source{base64}.data"), Kind::Missing),
        1
    );
    assert_eq!(
        report.count(&at("{image}.source{base64}.media_type"), Kind::Missing),
        1
    );
    assert_eq!(report.rows.len(), 2, "{report}");

    let report = claude_code_v3(false, Fault::Retext).check(&claude_code::TABLE, &control);
    let answer = "{assistant}.message{message}.content[]{text}.text";
    assert_eq!(report.count(answer, Kind::Missing), 1);
    assert_eq!(report.count("window:text/out/payload", Kind::Invented), 1);
    assert_eq!(report.rows.len(), 2, "{report}");

    let report = claude_code_v3(false, Fault::Swap).check(&claude_code::TABLE, &control);
    assert_eq!(report.count(&at("{text}.text"), Kind::Order), 1, "{report}");
    assert_eq!(report.rows.len(), 1, "{report}");

    let report = claude_code_v3(false, Fault::WrongDirection).check(&claude_code::TABLE, &control);
    assert_eq!(
        report.count("{user}.message.content", Kind::Changed),
        1,
        "{report}"
    );
    assert_eq!(report.rows.len(), 1, "{report}");

    let report = claude_code_v3(false, Fault::Extra).check(&claude_code::TABLE, &control);
    assert_eq!(
        report.count("window:text/in/payload", Kind::Invented),
        1,
        "{report}"
    );
    assert_eq!(report.rows.len(), 1, "{report}");
}

#[test]
fn the_gate_reports_attributes_it_does_not_know() {
    let control = claude_code_source(&[RESULT_TEXT, RESULT_IMAGE]);
    let mut chain = claude_code_v3(false, Fault::None);
    let leaf = *chain.nodes.last().unwrap();
    let stray = trible_genid_for_tests();
    let mut fragment = Fragment::empty();
    let value =
        fragment.put::<triblespace::prelude::blobencodings::UTF8String, _>("unmodelled".to_owned());
    fragment += Fragment::new(
        [],
        TribleSet::from_iter([Trible::force(&leaf, &stray, &value)]),
    );
    chain.fragment += fragment;
    let report = chain.check(&claude_code::TABLE, &control);
    assert_eq!(
        report.count(&format!("window:annotation/attr:{stray:X}"), Kind::Invented),
        1,
        "{report}"
    );
}

/// A fixed attribute id for the unknown-attribute test; never published.
fn trible_genid_for_tests() -> Id {
    triblespace::macros::id_hex!("0123456789ABCDEF0123456789ABCDEF")
}

#[test]
fn the_gate_passes_a_right_rendering_of_every_tool_result_item() {
    let witness =
        claude_code_source(&[RESULT_TEXT, RESULT_IMAGE, RESULT_REFERENCE, RESULT_DOCUMENT]);
    let report = claude_code_v3(true, Fault::None).check(&claude_code::TABLE, &witness);
    assert!(report.passed(), "{report}");
}

fn chatgpt_v3() -> Chain {
    use v3::fact::{direction::*, modality::*};
    let mut chain = Chain::new(SOURCE_CHATGPT);
    let role = |role: &str, time: Option<i128>| WitnessAnnotations {
        raw_role: Some(role.to_owned()),
        timestamp: time.map(unix).into_iter().collect(),
        ..WitnessAnnotations::default()
    };
    chain.push(Step {
        fact: said(SYSTEM, IN, "Be brief."),
        witness: role("system", None),
        ..Step::default()
    });
    chain.push(Step {
        fact: said(TEXT, IN, "Weather in Bremen?"),
        witness: role("user", Some(1772382841)),
        ..Step::default()
    });
    chain.push(Step {
        fact: FactCore {
            tool_name: Some("web".to_owned()),
            ..said(TOOL_CALL, OUT, "search(\"Bremen weather\")")
        },
        witness: role("assistant", Some(1772382842)),
        ..Step::default()
    });
    chain.push(Step {
        fact: FactCore {
            modality: Some(TOOL_RESULT),
            direction: Some(IN),
            ..FactCore::default()
        },
        witness: role("tool", Some(1772382843)),
        ..Step::default()
    });
    chain.push(Step {
        fact: said(TEXT, IN, "12 degrees, rain"),
        witness: role("tool", Some(1772382843)),
        ..Step::default()
    });
    let mut leaf = role("assistant", Some(1772382850));
    leaf.source_locator = Some("conv-1".to_owned());
    let answer = chain.push(Step {
        fact: said(TEXT, OUT, "Rainy, 12 degrees."),
        witness: leaf,
        ..Step::default()
    });
    chain.fragment += gloss(answer, v3::gloss::category::TITLE, "Synthetic weather").unwrap();
    chain
}

#[test]
fn the_gate_passes_a_right_rendering_of_a_chatgpt_conversation() {
    let report = chatgpt_v3().check(&chatgpt::TABLE, CHATGPT);
    assert!(report.passed(), "{report}");
}

// ---------------------------------------------------------------------------
// The witnesses: main's importers lose these today.

fn claude_code_main(source: &str) -> Report {
    let mut union = Fragment::empty();
    crate::archive_claude_code::project_bytes(
        "synthetic.jsonl",
        Bytes::from_source(source.as_bytes().to_vec()),
        |projected| {
            union += projected.fragment;
            Ok(())
        },
    )
    .unwrap();
    let blobs = reader(&mut union);
    check(
        &claude_code::TABLE,
        source.as_bytes(),
        union.facts(),
        &blobs,
    )
}

/// Main's Claude Code importer keeps text and image items of a tool result
/// and loses every other item: a `tool_reference` (the tool definition it
/// loaded) and a `document` (the file she was shown).
#[test]
fn witness_claude_code_loses_non_text_tool_result_items() {
    let control = claude_code_main(&claude_code_source(&[RESULT_TEXT, RESULT_IMAGE]));
    let witness = claude_code_main(&claude_code_source(&[
        RESULT_TEXT,
        RESULT_IMAGE,
        RESULT_REFERENCE,
        RESULT_DOCUMENT,
    ]));
    eprintln!("main, control:\n{control}\nmain, witness:\n{witness}");
    assert!(!witness.passed());

    for lost in [
        at("{tool_reference}.tool_name"),
        at("{document}.source{base64}.data"),
        at("{document}.source{base64}.media_type"),
    ] {
        assert_eq!(witness.count(&lost, Kind::Missing), 1, "{lost}\n{witness}");
        assert!(!control.names(&lost));
    }
    // The items main does keep are no finding in either run.
    for kept in [
        at("{image}.source{base64}.data"),
        at("{image}.source{base64}.media_type"),
    ] {
        assert!(!witness.names(&kept), "{kept}\n{witness}");
        assert!(!control.names(&kept), "{kept}\n{control}");
    }
    // A text item is kept, though under main's tool-result modality.
    assert_eq!(witness.count(&at("{text}.text"), Kind::Missing), 0);
    // Apart from the lost items, the two runs say the same.
    let rest = |report: &Report| -> Vec<(String, Kind)> {
        report
            .rows
            .iter()
            .filter(|row| {
                !row.path.contains("{tool_reference}") && !row.path.contains("{document}")
            })
            .map(|row| (row.path.clone(), row.kind))
            .collect()
    };
    assert_eq!(rest(&control), rest(&witness));
}

fn chatgpt_main(source: &str) -> Report {
    let mut union = Fragment::empty();
    crate::archive_chatgpt::project_bytes(
        "conversations.json",
        Bytes::from_source(source.as_bytes().to_vec()),
        &std::collections::BTreeMap::new(),
        |projected| {
            union += projected.fragment;
            Ok(())
        },
    )
    .unwrap();
    let blobs = reader(&mut union);
    check(&chatgpt::TABLE, source.as_bytes(), union.facts(), &blobs)
}

/// Main's ChatGPT importer reads `recipient` only to tell a call from an
/// answer: the name of the tool she addressed is lost.
#[test]
fn witness_chatgpt_loses_the_recipient() {
    let report = chatgpt_main(CHATGPT);
    eprintln!("main, chatgpt:\n{report}");
    assert_eq!(report.count(RECIPIENT, Kind::Missing), 1, "{report}");
    // Her call's input itself is kept, and so is every message's text.
    assert!(
        !report.names("[].mapping.<id>.message.content{code}.text"),
        "{report}"
    );
    assert_eq!(
        report.count(
            "[].mapping.<id>.message.content{text}.parts[]",
            Kind::Missing
        ),
        0,
        "{report}"
    );
}
