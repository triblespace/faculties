//! Identity tests for the v3 constructors, on synthetic data only.

use std::collections::BTreeSet;

use triblespace::core::blob::MemoryBlobStore;
use triblespace::core::import::json_tree::JsonTreeImporter;
use triblespace::core::metadata;
use triblespace::prelude::blobencodings::UTF8String;
use triblespace::prelude::inlineencodings::{Handle, NsTAIInterval, U256BE};

use super::*;
use crate::schemas::archive_v3::{fact::absent, fact::detail, fact::direction, fact::form};
use crate::schemas::archive_v3::{fact::modality, fact::register, fact::status};
use crate::schemas::archive_v3::{gloss::category, node::ends};
use crate::schemas::blockdag::source_projection::{SOURCE_CLAUDE_CODE, SOURCE_CLAUDE_WEB};

fn tree(json: &str) -> Fragment {
    let mut store = MemoryBlobStore::new();
    JsonTreeImporter::new(&mut store, None)
        .import_str(json)
        .expect("synthetic JSON imports")
}

fn text(dir: Id, payload: &str) -> FactCore {
    FactCore {
        modality: Some(modality::TEXT),
        direction: Some(dir),
        payload: Some(payload.to_owned()),
        ..FactCore::default()
    }
}

/// A synthetic window: what her window rendered, one fact per node.
fn window(call_arguments: &str, schema_json: &str) -> Vec<FactCore> {
    vec![
        FactCore {
            modality: Some(modality::SYSTEM),
            direction: Some(direction::IN),
            payload: Some("You are a careful assistant.".to_owned()),
            ..FactCore::default()
        },
        FactCore {
            modality: Some(modality::TOOL_DEFINITION),
            direction: Some(direction::IN),
            tool_name: Some("Read".to_owned()),
            tool_namespace: Some("synthetic-fs".to_owned()),
            payload: Some("Reads a file.".to_owned()),
            structure: Some(tree(schema_json)),
            ..FactCore::default()
        },
        text(direction::IN, "Please look at notes.txt"),
        FactCore {
            modality: Some(modality::THINKING),
            direction: Some(direction::OUT),
            blob: Some(anybytes::Bytes::from(vec![1u8, 2, 3, 4])),
            media_type: Some(
                MediaType::parse("application/vnd.anthropic.thinking-signature").unwrap(),
            ),
            ..FactCore::default()
        },
        FactCore {
            modality: Some(modality::TOOL_CALL),
            direction: Some(direction::OUT),
            tool_name: Some("Read".to_owned()),
            structure: Some(tree(call_arguments)),
            ..FactCore::default()
        },
        FactCore {
            modality: Some(modality::TOOL_RESULT),
            direction: Some(direction::IN),
            tool_name: Some("Read".to_owned()),
            status: Some(status::OK),
            exit_code: Some(0),
            ..FactCore::default()
        },
        FactCore {
            modality: Some(modality::FILE),
            direction: Some(direction::IN),
            payload: Some("synthetic contents\n".to_owned()),
            title: Some("notes.txt".to_owned()),
            source_uri: vec!["file:///synthetic/notes.txt".to_owned()],
            media_type: Some(MediaType::parse("text/plain; charset=utf-8").unwrap()),
            ..FactCore::default()
        },
        FactCore {
            modality: Some(modality::IMAGE),
            direction: Some(direction::IN),
            absent: Some(absent::BODY),
            detail: Some(detail::HIGH),
            ..FactCore::default()
        },
        FactCore {
            modality: Some(modality::AUDIO),
            direction: Some(direction::IN),
            blob: Some(anybytes::Bytes::from(vec![0u8; 8])),
            media_type: Some(MediaType::parse("audio/L16; rate=24000; channels=1").unwrap()),
            sense: Some("ear".to_owned()),
            ..FactCore::default()
        },
        FactCore {
            modality: Some(modality::TEXT),
            direction: Some(direction::OUT),
            payload: Some("Done: it says hello.".to_owned()),
            register: Some(register::ANSWER),
            ..FactCore::default()
        },
    ]
}

struct Built {
    facts: Vec<Id>,
    nodes: Vec<Id>,
    fragment: Fragment,
}

fn chain(window: Vec<FactCore>) -> Built {
    let mut built = Built {
        facts: Vec::new(),
        nodes: Vec::new(),
        fragment: Fragment::empty(),
    };
    for core in window {
        let held = fact(core).unwrap();
        built.facts.push(held.root().unwrap());
        let node = node(built.nodes.last().copied(), held).unwrap();
        built.nodes.push(node.root().unwrap());
        built.fragment += unexported(node);
    }
    built
}

fn ids(facts: &TribleSet) -> BTreeSet<Id> {
    facts.iter().map(|t| *t.e()).collect()
}

#[test]
fn identical_content_converges_across_builds() {
    let a = chain(window(
        r#"{"path": "notes.txt", "limit": 10}"#,
        r#"{"type": "object", "properties": {"path": {"type": "string"}}}"#,
    ));
    // The same information spelled differently: member order and whitespace
    // of the tool-defined JSON, and the media type's case and parameter
    // order, are spelling, not information.
    let b = chain(window(
        r#"{ "limit":10,"path":"notes.txt" }"#,
        r#"{"properties":{"path":{"type":"string"}},"type":"object"}"#,
    ));
    assert_eq!(a.facts, b.facts);
    assert_eq!(a.nodes, b.nodes);
    assert_eq!(a.fragment.facts(), b.fragment.facts());

    // Different information is a different id.
    let c = chain(window(
        r#"{"path": "notes.txt", "limit": 11}"#,
        r#"{"type": "object", "properties": {"path": {"type": "string"}}}"#,
    ));
    assert_eq!(a.facts[..4], c.facts[..4]);
    assert_ne!(a.facts[4], c.facts[4]);
    assert_eq!(a.nodes[..4], c.nodes[..4]);
    assert_ne!(a.nodes[4..], c.nodes[4..]);
}

#[test]
fn annotations_never_change_an_id() {
    let plain = chain(window(r#"{"path": "notes.txt"}"#, r#"{"type": "object"}"#));
    let annotated = chain(window(r#"{"path": "notes.txt"}"#, r#"{"type": "object"}"#));
    let mut annotations = annotated.fragment.clone();

    let call = annotated.nodes[4];
    let result = annotated.nodes[5];
    let answer = *annotated.nodes.last().unwrap();
    annotations += annotate_node(
        result,
        &NodeAnnotations {
            responds_to: vec![call],
            ..NodeAnnotations::default()
        },
    );
    annotations += annotate_node(
        answer,
        &NodeAnnotations {
            ends: vec![ends::END_TURN],
            recalls: vec![annotated.nodes[2]],
            ..NodeAnnotations::default()
        },
    );
    annotations += gloss(answer, category::TITLE, "A synthetic title").unwrap();
    annotations += citation(answer, 0, 5, "https://example.invalid/cited").unwrap();
    annotations += MediaType::parse("text/plain; charset=utf-8")
        .unwrap()
        .annotations();

    let bare_witness = witness(SOURCE_CLAUDE_CODE, answer).unwrap();
    let witness_id = bare_witness.root().unwrap();
    let time: Inline<NsTAIInterval> = (
        hifitime::Epoch::from_unix_seconds(1.0e9),
        hifitime::Epoch::from_unix_seconds(1.0e9),
    )
        .try_to_inline()
        .unwrap();
    let claims = occurrence(
        witness_id,
        WitnessAnnotations {
            generated_by: vec![Model::vendor("synthetic-model-1").unwrap()],
            raw_model: Some("synthetic-model-1".to_owned()),
            raw_role: Some("assistant".to_owned()),
            source_locator: Some("synthetic-session".to_owned()),
            timestamp: vec![time],
            pointer: vec!["synthetic-pointer".to_owned()],
            claimed_size: Some(4096),
            claimed_media_type: Some(MediaType::parse("image/png").unwrap()),
            claimed_title: Some("unseen.png".to_owned()),
            compaction_summary: true,
            ..WitnessAnnotations::default()
        },
    )
    .unwrap();
    let occurrence_id = claims.root().unwrap();
    annotations += unexported(claims);

    // Every annotation landed, and nothing it added is a fact or node of the
    // chain or carries a new id the chain depends on.
    let annotated_ids = ids(annotations.facts());
    let mut union = annotations.facts().clone();
    union += plain.fragment.facts().clone();
    assert_eq!(union.len(), annotations.facts().len());
    assert_eq!(plain.nodes, annotated.nodes);
    assert_eq!(plain.facts, annotated.facts);
    assert!(annotated_ids.contains(&occurrence_id));

    // The witness keeps its id however much a source says about it.
    assert_eq!(
        witness(SOURCE_CLAUDE_CODE, answer).unwrap().root(),
        Some(witness_id)
    );
    // And the annotations are queryable.
    let responds: Vec<Id> = find!(
        call: Id,
        pattern!(annotations.facts(), [{ result @ schema::node::responds_to: ?call }])
    )
    .collect();
    assert_eq!(responds, vec![call]);
    let sizes: Vec<Inline<U256BE>> = find!(
        size: Inline<U256BE>,
        pattern!(annotations.facts(), [{ occurrence_id @ schema::witness::claimed_size: ?size }])
    )
    .collect();
    assert_eq!(sizes.len(), 1);
    let tags: BTreeSet<Id> = find!(
        tag: Id,
        pattern!(annotations.facts(), [{ occurrence_id @ metadata::tag: ?tag }])
    )
    .collect();
    assert!(tags.contains(&schema::witness::COMPACTION_SUMMARY));
    // The claims are one occurrence of the witness, never on the witness.
    let of: Vec<Id> = find!(
        witness: Id,
        pattern!(annotations.facts(), [{ occurrence_id @ schema::occurrence::of: ?witness }])
    )
    .collect();
    assert_eq!(of, vec![witness_id]);
}

/// Critique F1: the same lived content converges whether or not a source
/// also claims something about it the window did not render.
#[test]
fn source_only_claims_never_split_lived_content() {
    let lived = || {
        vec![
            text(direction::IN, "Here is my photo."),
            FactCore {
                modality: Some(modality::IMAGE),
                direction: Some(direction::IN),
                absent: Some(absent::BODY),
                ..FactCore::default()
            },
            text(direction::OUT, "What a lovely sky."),
        ]
    };
    // An export that claims the asset's size, media type, name and pointer.
    let export = chain(lived());
    let mut export_fragment = export.fragment.clone();
    for (index, node) in export.nodes.iter().enumerate() {
        let witness = witness(SOURCE_CLAUDE_WEB, *node).unwrap();
        let witness_id = witness.root().unwrap();
        export_fragment += witness;
        let claims = if index == 1 {
            WitnessAnnotations {
                pointer: vec!["synthetic-file-uuid".to_owned()],
                claimed_size: Some(123_456),
                claimed_media_type: Some(MediaType::parse("image/jpeg").unwrap()),
                claimed_title: Some("IMG_0001.jpg".to_owned()),
                ..WitnessAnnotations::default()
            }
        } else {
            WitnessAnnotations::default()
        };
        export_fragment += unexported(occurrence(witness_id, claims).unwrap());
    }
    // A transcript of the same window that records none of it.
    let transcript = chain(lived());

    assert_eq!(export.facts, transcript.facts);
    assert_eq!(export.nodes, transcript.nodes);
    let shared: BTreeSet<Id> = ids(transcript.fragment.facts());
    assert!(shared.is_subset(&ids(export_fragment.facts())));

    // Control: a title her window DID render is lived content, so it is core
    // and the rendering table, not the file, made the difference.
    let shown = fact(FactCore {
        title: Some("IMG_0001.jpg".to_owned()),
        ..lived().remove(1)
    })
    .unwrap();
    assert_ne!(shown.root(), Some(export.facts[1]));
}

/// Critique F2: `continues` and `spawned_by` are annotations, so a root's id
/// never depends on another unit or on the order units are imported in.
#[test]
fn continues_and_spawned_by_leave_root_ids_alone() {
    let context = || {
        vec![
            FactCore {
                modality: Some(modality::SYSTEM),
                direction: Some(direction::IN),
                payload: Some("You are a subagent.".to_owned()),
                ..FactCore::default()
            },
            text(direction::IN, "Count the synthetic files."),
            text(direction::OUT, "There are three."),
        ]
    };
    // Imported alone: no knowledge of the parent unit.
    let alone = chain(context());

    // Imported after the parent: the parent's call node and an older tail
    // are known and annotate the root.
    let parent = chain(vec![
        text(direction::IN, "Start a helper."),
        FactCore {
            modality: Some(modality::TOOL_CALL),
            direction: Some(direction::OUT),
            tool_name: Some("Task".to_owned()),
            structure: Some(tree(r#"{"prompt": "Count the synthetic files."}"#)),
            ..FactCore::default()
        },
    ]);
    let call = parent.nodes[1];
    let old_tail = parent.nodes[0];
    let together = chain(context());
    let root = together.nodes[0];
    let mut fragment = together.fragment.clone();
    fragment += parent.fragment.clone();
    fragment += annotate_node(
        root,
        &NodeAnnotations {
            continues: vec![old_tail],
            spawned_by: vec![call],
            ..NodeAnnotations::default()
        },
    );

    assert_eq!(alone.nodes, together.nodes);
    assert_eq!(alone.facts, together.facts);
    let spawned: Vec<Id> = find!(
        call: Id,
        pattern!(fragment.facts(), [{ root @ schema::node::spawned_by: ?call }])
    )
    .collect();
    assert_eq!(spawned, vec![call]);
    let continued: Vec<Id> = find!(
        tail: Id,
        pattern!(fragment.facts(), [{ root @ schema::node::continues: ?tail }])
    )
    .collect();
    assert_eq!(continued, vec![old_tail]);
}

#[test]
fn media_types_keep_parameters_canonically() {
    let parse = |s: &str| MediaType::parse(s).unwrap().canonical();
    assert_eq!(
        parse("Audio/L16; rate=24000; channels=1"),
        "audio/l16;channels=1;rate=24000"
    );
    assert_eq!(
        parse("audio/l16;channels=\"1\";rate=24000"),
        "audio/l16;channels=1;rate=24000"
    );
    assert_eq!(parse("text/plain;"), "text/plain");
    assert_eq!(
        parse(r#"text/plain; title="a \"b\" c""#),
        r#"text/plain;title="a \"b\" c""#
    );
    // Values are exact: case is information here.
    assert_ne!(
        parse("text/plain;charset=UTF-8"),
        parse("text/plain;charset=utf-8")
    );
    assert_eq!(
        MediaType::parse("TEXT/Plain; Charset=UTF-8")
            .unwrap()
            .essence(),
        "text/plain"
    );
    for bad in [
        "",
        "  ",
        "text",
        "text/",
        "text/plain; a",
        "text/plain;a=1;A=2",
        "text/plain;a=\"x",
    ] {
        assert!(MediaType::parse(bad).is_err(), "{bad:?} must be refused");
    }

    // A parameter-free type is the Files entity Files already builds.
    assert_eq!(
        MediaType::parse("Image/PNG").unwrap().core().root(),
        crate::files::media_type_fragment("image/png")
            .unwrap()
            .root()
    );
    // Parameters are identity of the media type.
    assert_ne!(
        MediaType::parse("audio/l16;rate=24000")
            .unwrap()
            .core()
            .root(),
        MediaType::parse("audio/l16;rate=16000")
            .unwrap()
            .core()
            .root()
    );
    // The essence is linked as an annotation.
    let media_type = MediaType::parse("audio/l16;rate=24000").unwrap();
    let root = media_type.core().root().unwrap();
    let essence = MediaType::parse("audio/l16")
        .unwrap()
        .core()
        .root()
        .unwrap();
    let annotations = media_type.annotations();
    assert_eq!(annotations.exports().count(), 0);
    let linked: Vec<Id> = find!(
        e: Id,
        pattern!(annotations.facts(), [{ root @ schema::media_type::essence: ?e }])
    )
    .collect();
    assert_eq!(linked, vec![essence]);
}

#[test]
fn constructors_refuse_what_was_never_rendered() {
    // Only a span gap may be of unknown kind or direction.
    assert!(fact(FactCore {
        absent: Some(absent::SPAN),
        ..FactCore::default()
    })
    .is_ok());
    assert!(fact(FactCore {
        modality: Some(modality::TEXT),
        absent: Some(absent::BODY),
        ..FactCore::default()
    })
    .is_err());
    assert!(fact(FactCore {
        structure: Some(Fragment::empty()),
        ..text(direction::IN, "x")
    })
    .is_err());
    assert!(node(None, Fragment::empty()).is_err());
    assert!(gloss(genid().id, category::RECAP, "").is_err());
    assert!(citation(genid().id, 5, 4, "https://example.invalid").is_err());
}

/// Constructor delta for the native writer: an empty text that reached her
/// window is a fact holding the empty blob. It differs from a body gap (its
/// content unknown) and from a fact with no payload (a part the provider
/// never rendered).
#[test]
fn an_empty_rendered_text_is_a_fact_holding_the_empty_blob() {
    use triblespace::core::repo::BlobStoreGet;
    let empty = fact(text(direction::IN, "")).unwrap();
    let gap = fact(FactCore {
        modality: Some(modality::TEXT),
        direction: Some(direction::IN),
        absent: Some(absent::BODY),
        ..FactCore::default()
    })
    .unwrap();
    let unrendered = fact(FactCore {
        modality: Some(modality::TEXT),
        direction: Some(direction::IN),
        ..FactCore::default()
    })
    .unwrap();
    assert_ne!(empty.root(), gap.root());
    assert_ne!(empty.root(), unrendered.root());
    assert_eq!(empty.root(), fact(text(direction::IN, "")).unwrap().root());
    let id = empty.root().unwrap();
    let facts = empty.facts();
    let payloads: Vec<Inline<Handle<UTF8String>>> = find!(
        payload: Inline<Handle<UTF8String>>,
        pattern!(facts, [{ id @ schema::fact::payload: ?payload }])
    )
    .collect();
    assert_eq!(payloads.len(), 1);
    let mut blobs = empty.clone();
    let reader = blobs.blobs_mut().snapshot().unwrap();
    let held: anybytes::View<str> = reader.get(payloads[0]).unwrap();
    assert_eq!(held.as_ref(), "");
}

/// Constructor delta for the native writer: `generated_by` references a
/// model entity. The occurrence takes its id and the fragment that builds
/// it, so an exact model-selection fragment rides along unchanged; an
/// importer passes the vendor model of the provider's label. A fragment
/// that does not build the entity it names is refused.
#[test]
fn generated_by_references_a_model_entity() {
    let witness_id = witness(SOURCE_CLAUDE_CODE, genid().id)
        .unwrap()
        .root()
        .unwrap();
    // A model selection: the vendor model's tag and name, and a setting.
    let mut selection = Fragment::empty();
    let name = selection.put::<UTF8String, _>("synthetic-model-1".to_owned());
    selection += entity! { _ @
        metadata::tag: &crate::schemas::blockdag::vendor_model::KIND,
        metadata::name: name,
        schema::fact::effort: &schema::fact::effort::HIGH,
    };
    let selected = Model {
        id: selection.root().unwrap(),
        fragment: selection.clone(),
    };
    let vendor = Model::vendor("synthetic-model-1").unwrap();
    assert_eq!(
        vendor.id,
        crate::blockdag::vendor_model("synthetic-model-1")
            .root()
            .unwrap()
    );
    assert_ne!(selected.id, vendor.id);
    for model in [&selected, &vendor] {
        let built = occurrence(
            witness_id,
            WitnessAnnotations {
                generated_by: vec![model.clone()],
                ..WitnessAnnotations::default()
            },
        )
        .unwrap();
        let root = built.root().unwrap();
        let facts = built.facts();
        let named: Vec<Id> = find!(
            model: Id,
            pattern!(facts, [{ root @ schema::witness::generated_by: ?model }])
        )
        .collect();
        assert_eq!(named, vec![model.id]);
        // The model's own tribles ride along, exactly.
        let mut union = facts.clone();
        union += model.fragment.facts().clone();
        assert_eq!(union.len(), facts.len());
        assert_eq!(built.exports().collect::<Vec<_>>(), vec![root]);
    }
    // Two models are two occurrences.
    let of = |model: &Model| {
        occurrence(
            witness_id,
            WitnessAnnotations {
                generated_by: vec![model.clone()],
                ..WitnessAnnotations::default()
            },
        )
        .unwrap()
        .root()
    };
    assert_ne!(of(&selected), of(&vendor));
    // A fragment that does not build the entity it names.
    let dangling = Model {
        id: genid().id,
        fragment: selection,
    };
    assert!(occurrence(
        witness_id,
        WitnessAnnotations {
            generated_by: vec![dangling.clone()],
            ..WitnessAnnotations::default()
        },
    )
    .is_err());
    // A gloss names its model by the same path.
    let node = chain(vec![text(direction::OUT, "x")]).nodes[0];
    let g = gloss(node, category::RECAP, "summary").unwrap();
    let gloss_id = g.root().unwrap();
    let annotated = annotate_gloss(gloss_id, &selected).unwrap();
    let facts = annotated.facts();
    let wrote: Vec<Id> = find!(
        model: Id,
        pattern!(facts, [{ gloss_id @ schema::gloss::generated_by: ?model }])
    )
    .collect();
    assert_eq!(wrote, vec![selected.id]);
    assert!(annotate_gloss(gloss_id, &dangling).is_err());
}

#[test]
fn form_voiced_is_not_her_text() {
    // Harness text in her slot is `in` with `form: voiced`: a different fact
    // from the same words spoken by her.
    let voiced = fact(FactCore {
        form: Some(form::VOICED),
        ..text(direction::IN, "API Error: synthetic")
    })
    .unwrap();
    let hers = fact(text(direction::OUT, "API Error: synthetic")).unwrap();
    assert_ne!(voiced.root(), hers.root());
}

#[test]
fn senses_glosses_and_vocabulary_are_intrinsic() {
    assert_eq!(sense("ear").root(), sense("ear").root());
    assert_ne!(sense("ear").root(), sense("Ear").root());

    let node = chain(vec![text(direction::OUT, "x")]).nodes[0];
    let a = gloss(node, category::REASONING_SUMMARY, "summary").unwrap();
    let mut b = gloss(node, category::REASONING_SUMMARY, "summary").unwrap();
    let root = b.root().unwrap();
    b += annotate_gloss(root, &Model::vendor("synthetic-summarizer").unwrap()).unwrap();
    assert_eq!(a.root(), b.root());
    assert_ne!(
        gloss(node, category::RECAP, "summary").unwrap().root(),
        a.root()
    );

    // Every vocabulary term has its own id and a name.
    let vocabulary = vocabulary_fragment();
    let named: BTreeSet<Id> = find!(
        (term: Id, name: Inline<Handle<UTF8String>>),
        pattern!(vocabulary.facts(), [{ ?term @ metadata::name: ?name }])
    )
    .map(|(term, _)| term)
    .collect();
    let mut all: Vec<Id> = Vec::new();
    for specs in [
        schema::fact::modality::SPECS,
        schema::fact::direction::SPECS,
        schema::fact::status::SPECS,
        schema::fact::detail::SPECS,
        schema::fact::register::SPECS,
        schema::fact::form::SPECS,
        schema::fact::absent::SPECS,
        schema::fact::effort::SPECS,
        schema::node::rendering::SPECS,
        schema::gloss::category::SPECS,
    ] {
        all.extend(specs.iter().map(|(id, _)| *id));
    }
    all.extend(ends::SPECS.iter().map(|(id, _, _)| *id));
    let distinct: BTreeSet<Id> = all.iter().copied().collect();
    assert_eq!(distinct.len(), all.len());
    assert_eq!(named, distinct);
    let finals: BTreeSet<Id> = find!(
        (term: Id, last: bool),
        pattern!(vocabulary.facts(), [{ ?term @ schema::node::ends::message_final: ?last }])
    )
    .filter(|(_, last)| *last)
    .map(|(term, _)| term)
    .collect();
    assert!(finals.contains(&ends::MAX_TOKENS));
    assert!(!finals.contains(&ends::CUT));
}

/// M2: an occurrence's id holds only its own record's claims. Why the
/// sampling or the turn ended annotates it, so learning it from a later
/// record keeps the id and adds only the annotation.
#[test]
fn an_occurrence_keeps_its_id_when_its_end_is_learned() {
    // Minted with `trible genid`; it names nothing outside this test.
    let node = triblespace::macros::id_hex!("3C2F6478399354F035CC65B45A17FD7D");
    let witness_id = witness(SOURCE_CLAUDE_CODE, node).unwrap().root().unwrap();
    let claims = WitnessAnnotations {
        raw_role: Some("assistant".to_owned()),
        source_locator: Some("synthetic-session".to_owned()),
        ..WitnessAnnotations::default()
    };
    let bare = occurrence(witness_id, claims.clone()).unwrap();
    let ended = occurrence(
        witness_id,
        WitnessAnnotations {
            ends: vec![ends::INTERRUPTED],
            turn_ends: vec![ends::INTERRUPTED],
            ..claims
        },
    )
    .unwrap();
    assert_eq!(bare.root(), ended.root());
    let added = ended.facts().difference(bare.facts());
    assert_eq!(added.len(), 2, "only the two end annotations are new");
    let id = ended.root().unwrap();
    let facts = ended.facts();
    let ended_by: Vec<Id> = find!(
        term: Id,
        pattern!(facts, [{ id @ schema::witness::ends: ?term }])
    )
    .collect();
    assert_eq!(ended_by, vec![ends::INTERRUPTED]);
}

/// P1: the M2 refusal compares the records' own claim values, as the store
/// would merge their occurrences: the same node and the same claims, with
/// repeated claims as sets. Each verdict agrees with whether the store
/// would merge the two (checked here, in the test only, by their ids).
#[test]
fn p1_ends_disagree_compares_the_records_own_claims() {
    // Minted with `trible genid`; they name nothing outside this test.
    let node = triblespace::macros::id_hex!("B8384CF1C78653D455C40AFCD841CD67");
    let other_node = triblespace::macros::id_hex!("6E3430BD7CBE3047A3EAA581CF538AAF");
    let at = |second: f64| -> Inline<NsTAIInterval> {
        let epoch = hifitime::Epoch::from_unix_seconds(1.0e9 + second);
        (epoch, epoch).try_to_inline().unwrap()
    };
    let base = WitnessAnnotations {
        raw_role: Some("assistant".to_owned()),
        raw_author: vec!["one".to_owned(), "two".to_owned()],
        source_locator: Some("synthetic-session".to_owned()),
        timestamp: vec![at(1.0)],
        ..WitnessAnnotations::default()
    };
    let ended = |claims: &WitnessAnnotations, by: Vec<Id>, turn_by: Vec<Id>| WitnessAnnotations {
        ends: by,
        turn_ends: turn_by,
        ..claims.clone()
    };
    let merged = |a: &(Id, WitnessAnnotations), b: &(Id, WitnessAnnotations)| {
        let id = |(node, claims): &(Id, WitnessAnnotations)| {
            let witness_id = witness(SOURCE_CLAUDE_CODE, *node).unwrap().root().unwrap();
            occurrence(witness_id, claims.clone())
                .unwrap()
                .root()
                .unwrap()
        };
        id(a) == id(b)
    };
    let reordered = WitnessAnnotations {
        raw_author: vec!["two".to_owned(), "one".to_owned(), "two".to_owned()],
        ..base.clone()
    };
    let later = WitnessAnnotations {
        timestamp: vec![at(2.0)],
        ..base.clone()
    };
    let other_role = WitnessAnnotations {
        raw_role: Some("user".to_owned()),
        ..base.clone()
    };
    let end_turn = vec![ends::END_TURN];
    let max_tokens = vec![ends::MAX_TOKENS];
    // (first, second, whether they disagree)
    let cases = [
        // One occurrence, two ends: refused.
        (
            (node, ended(&base, end_turn.clone(), vec![])),
            (node, ended(&base, max_tokens.clone(), vec![])),
            true,
        ),
        // Repeated claims are sets, as in the store.
        (
            (node, ended(&base, end_turn.clone(), vec![])),
            (node, ended(&reordered, max_tokens.clone(), vec![])),
            true,
        ),
        // A turn end learned for one of them only.
        (
            (node, ended(&base, vec![], vec![ends::INTERRUPTED])),
            (node, base.clone()),
            true,
        ),
        // The same ends, in another order: one occurrence, no conflict.
        (
            (
                node,
                ended(&base, vec![ends::END_TURN, ends::ERROR], vec![]),
            ),
            (
                node,
                ended(&base, vec![ends::ERROR, ends::END_TURN], vec![]),
            ),
            false,
        ),
        // Another time, another role or another node: two occurrences.
        (
            (node, ended(&base, end_turn.clone(), vec![])),
            (node, ended(&later, max_tokens.clone(), vec![])),
            false,
        ),
        (
            (node, ended(&base, end_turn.clone(), vec![])),
            (node, ended(&other_role, max_tokens.clone(), vec![])),
            false,
        ),
        (
            (node, ended(&base, end_turn.clone(), vec![])),
            (other_node, ended(&base, max_tokens.clone(), vec![])),
            false,
        ),
    ];
    for (at, (first, second, disagree)) in cases.into_iter().enumerate() {
        let one = merged(&first, &second);
        let same_ends = {
            let set = |ids: &[Id]| ids.iter().copied().collect::<BTreeSet<Id>>();
            set(&first.1.ends) == set(&second.1.ends)
                && set(&first.1.turn_ends) == set(&second.1.turn_ends)
        };
        assert_eq!(
            disagree,
            one && !same_ends,
            "case {at}: the store's verdict"
        );
        assert_eq!(
            ends_disagree(&[first.clone(), second.clone()]),
            disagree,
            "case {at}"
        );
        // Among others, in either order.
        let filler = (other_node, ended(&later, end_turn.clone(), vec![]));
        assert_eq!(
            ends_disagree(&[second, filler, first]),
            disagree,
            "case {at}, among others"
        );
    }
}

#[test]
fn a_window_is_its_root_renderer_and_model_and_never_changes_a_node() {
    let root = node([], fact(text(direction::IN, "wake")).unwrap()).unwrap();
    let root_id = root.root().unwrap();
    let before = root.facts().clone();
    let renderer =
        super::renderer("{{ messages }}", "{\"model\":{}}", "drive-codec@abc123").unwrap();
    let renderer_id = renderer.root().unwrap();
    let nemotron = Model::vendor("nemotron-test").unwrap();
    let other = Model::vendor("other-test").unwrap();

    let a = crate::archive_v3::window(root_id, renderer_id, &nemotron).unwrap();
    let again = crate::archive_v3::window(root_id, renderer_id, &nemotron).unwrap();
    assert_eq!(a.root(), again.root(), "the same window converges");
    assert_eq!(a.facts(), again.facts());

    // The same moments read by another model, or rendered another way, are
    // another window, and the node is untouched.
    let b = crate::archive_v3::window(root_id, renderer_id, &other).unwrap();
    assert_ne!(a.root(), b.root());
    let renderer2 =
        super::renderer("{{ messages }}\n", "{\"model\":{}}", "drive-codec@abc123").unwrap();
    let c = crate::archive_v3::window(root_id, renderer2.root().unwrap(), &nemotron).unwrap();
    assert_ne!(a.root(), c.root());
    assert_ne!(renderer.root(), renderer2.root());
    assert_eq!(root.facts(), &before);
    assert_eq!(
        node([], fact(text(direction::IN, "wake")).unwrap())
            .unwrap()
            .root(),
        Some(root_id)
    );

    // The model entity rides along with the window.
    assert!(a.facts().len() > nemotron.fragment.facts().len());
    assert!(
        super::renderer("t", "k", "").is_err(),
        "a renderer names its codec"
    );
}

#[test]
fn unrendered_status_and_uri_are_occurrence_claims_not_fact_core() {
    let result = fact(text(direction::IN, "permission denied")).unwrap();
    let fact_id = result.root().unwrap();
    let held = node([], result).unwrap();
    let node_id = held.root().unwrap();
    let w = witness(SOURCE_CLAUDE_CODE, node_id)
        .unwrap()
        .root()
        .unwrap();
    let plain = occurrence(w, WitnessAnnotations::default()).unwrap();
    let claimed = occurrence(
        w,
        WitnessAnnotations {
            claimed_status: Some(status::ERROR),
            claimed_exit_code: Some(-9),
            raw_media_type: Some("image/png;;".to_owned()),
            claimed_uri: vec!["file:///tmp/x".to_owned()],
            ..WitnessAnnotations::default()
        },
    )
    .unwrap();
    assert_ne!(
        plain.root(),
        claimed.root(),
        "the claims are the record's own"
    );
    // The fact and node her window held are untouched by what it did not show.
    assert_eq!(
        node([], fact(text(direction::IN, "permission denied")).unwrap())
            .unwrap()
            .root(),
        Some(node_id)
    );
    assert_eq!(
        fact(text(direction::IN, "permission denied"))
            .unwrap()
            .root(),
        Some(fact_id)
    );
}

#[test]
fn two_originals_that_render_alike_share_the_fact_and_keep_two_occurrences() {
    let definition = fact(text(direction::IN, "exec: run a command")).unwrap();
    let held = node([], definition).unwrap();
    let node_id = held.root().unwrap();
    let w = witness(SOURCE_CLAUDE_CODE, node_id)
        .unwrap()
        .root()
        .unwrap();
    let padded = tree(r#"{"name":"exec","description":"run a command   "}"#);
    let plain = tree(r#"{"name":"exec","description":"run a command"}"#);
    let a = occurrence(
        w,
        WitnessAnnotations {
            rendered_from: Some(padded.clone()),
            ..WitnessAnnotations::default()
        },
    )
    .unwrap();
    let b = occurrence(
        w,
        WitnessAnnotations {
            rendered_from: Some(plain),
            ..WitnessAnnotations::default()
        },
    )
    .unwrap();
    assert_ne!(
        a.root(),
        b.root(),
        "each original is its own record's claim"
    );
    // The original tree rides along with the occurrence that claims it.
    assert!(padded.facts().iter().all(|t| a.facts().contains(t)));
    // The node her window held is the same for both.
    assert_eq!(
        node(
            [],
            fact(text(direction::IN, "exec: run a command")).unwrap()
        )
        .unwrap()
        .root(),
        Some(node_id)
    );
}
