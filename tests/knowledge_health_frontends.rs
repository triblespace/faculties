use faculties::knowledge::health::{self, KnowledgeHealth};
use faculties::knowledge::Knowledge;
use faculties::mcp::{Faculty, InvalidArguments};
use faculties::out::{Out, Part};
use faculties::storage::initialize_signer;
use serde_json::json;
use std::process::Command;

#[test]
fn observations_measure_the_knowledge_frontier_and_both_frontends_present_them() {
    let directory = tempfile::tempdir().unwrap();
    let pile = directory.path().join("research.pile");
    let key = directory.path().join("research.key");
    std::fs::File::create(&pile).unwrap();
    initialize_signer(&pile, Some(&key)).unwrap();
    let knowledge = Knowledge::new(pile.clone(), Some(key.clone()));
    let target = knowledge
        .create("refuted", "old claim", &["refuted".into()], true)
        .unwrap();
    let source = knowledge
        .create(
            "source",
            &format!("#link(\"wiki:{target:x}\")[claim]"),
            &[],
            true,
        )
        .unwrap();
    knowledge.create("alone", "孤独", &[], true).unwrap();
    let research = KnowledgeHealth::new(pile.clone(), Some(key.clone()));
    let health = research.health().unwrap();
    assert_eq!((health.entries, health.states, health.forks), (3, 3, 0));
    assert_eq!(
        (
            health.links.total,
            health.links.unique,
            health.links.missing
        ),
        (1, 1, 0)
    );
    assert_eq!(health.unanimous_orphans, 2);
    let tags = research.tags().unwrap();
    assert_eq!(
        (tags[0].tag.as_str(), tags[0].states, tags[0].entries),
        ("refuted", 1, 1)
    );
    assert_eq!(research.quality().unwrap()[0].revision, target);
    assert_eq!(research.hubs(1).unwrap().rows[0].entry.id, target);
    assert_eq!(research.risk().unwrap().rows[0].entry.id, source);
    assert_eq!(research.orphans(1).unwrap().total, 2);
    assert_eq!(research.orphans(1).unwrap().rows.len(), 1);
    assert!(research.hubs(0).unwrap().rows.is_empty());

    let adapter = health::mcp::KnowledgeHealth::new(pile.clone(), Some(key.clone()));
    for (verb, tool) in [
        (&["health"][..], "knowledge_health"),
        (&["tag", "counts"], "knowledge_tag_counts"),
        (&["quality"], "knowledge_quality"),
        (&["hubs"], "knowledge_hubs"),
        (&["risk"], "knowledge_risk"),
        (&["orphans"], "knowledge_orphans"),
    ] {
        let mut text = String::new();
        adapter
            .call(
                tool,
                serde_json::to_vec(&json!({})).unwrap().into(),
                &mut Out::new(&mut |part| {
                    let Part::Text { text: value } = part else {
                        panic!("non-text Knowledge health report")
                    };
                    text.push_str(&value);
                    Ok(())
                }),
            )
            .unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_knowledge"))
            .arg("--pile")
            .arg(&pile)
            .arg("--key")
            .arg(&key)
            .args(verb)
            .env_remove("DRIVE_ENDPOINT")
            .env_remove("DRIVE_KEY")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(String::from_utf8(output.stdout).unwrap(), text, "{tool}");
        assert!(!text.is_empty());
    }
}

#[test]
fn discovery_and_malformed_limits_do_not_touch_storage() {
    let directory = tempfile::tempdir().unwrap();
    let pile = directory.path().join("absent.pile");
    let research = health::mcp::KnowledgeHealth::new(pile.clone(), None);
    assert_eq!(research.tools().len(), 6);
    for (tool, arguments) in [
        ("knowledge_health", r#"{"pile":"/elsewhere"}"#),
        ("knowledge_hubs", r#"{"top":-1}"#),
        ("knowledge_orphans", r#"{"top":3,"top":4}"#),
        ("knowledge_orphans", r#"{"ids":true}"#),
    ] {
        let error = research
            .call(
                tool,
                arguments.to_owned().into(),
                &mut Out::new(&mut |_| panic!("unexpected report")),
            )
            .unwrap_err();
        assert!(
            error.downcast_ref::<InvalidArguments>().is_some(),
            "{error:#}"
        );
        assert!(!pile.exists());
    }
}
