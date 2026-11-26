//! Model-free fixtures: provider transcripts projected by the real importers,
//! runs written by the real launch and settlement fragments.

use super::*;
use crate::schemas::archive_v3::context;
use crate::spawn::{run_fragment, settlement_fragment, spawn_fragment, Exit, RunFacts};
use anybytes::Bytes;
use hifitime::Epoch;
use triblespace::core::blob::encodings::succinctarchive::{
    OrderedUniverse, SuccinctArchive, UnionArchive,
};
use triblespace::core::blob::MemoryBlobStore;
use triblespace::core::repo::BlobStore;

fn at(seconds: u64) -> super::super::IntervalValue {
    crate::clock::point(Epoch::from_unix_seconds(1_800_000_000.0 + seconds as f64)).unwrap()
}

fn archive(facts: &TribleSet) -> FactArchive {
    UnionArchive::new(vec![SuccinctArchive::<OrderedUniverse>::from(facts)])
}

#[derive(Default)]
struct Fixture {
    spawn: TribleSet,
    archive: TribleSet,
    blobs: MemoryBlobStore,
}

impl Fixture {
    fn add(set: &mut TribleSet, blobs: &mut MemoryBlobStore, fragment: Fragment) {
        let (facts, more) = fragment.into_facts_and_blobs();
        *set += facts;
        blobs.union(more);
    }

    fn spawn(&mut self, fragment: Fragment) {
        Self::add(&mut self.spawn, &mut self.blobs, fragment);
    }

    fn archived(&mut self, fragment: Fragment) {
        Self::add(&mut self.archive, &mut self.blobs, fragment);
    }

    /// A run of the zooid `brief`, prepared with `prompt`; returns the run.
    fn run(&mut self, brief: &str, prompt: &str, provider: &str, started: u64) -> Id {
        self.run_resuming(brief, prompt, provider, started, None)
    }

    fn run_resuming(
        &mut self,
        brief: &str,
        prompt: &str,
        provider: &str,
        started: u64,
        resumes: Option<Id>,
    ) -> Id {
        let (fragment, spawn, ..) = spawn_fragment(brief).unwrap();
        self.spawn(fragment);
        let (fragment, run) = run_fragment(&RunFacts {
            spawn,
            started_at: at(started),
            persona: *rngid(),
            spawner: *rngid(),
            unit: format!("spawn-{started}.service"),
            workdir: "/tmp".to_owned(),
            effort: "high".to_owned(),
            prompt: prompt.to_owned(),
            provider: provider.to_owned(),
            provider_reason: "asked".to_owned(),
            model: None,
            model_unset: Some("none".to_owned()),
            untinted: None,
            resumes,
            cover: None,
            resident: false,
        });
        self.spawn(fragment);
        run
    }

    fn settle(&mut self, run: Id, finished: u64) {
        let (fragment, _) = settlement_fragment(
            run,
            at(finished),
            &Exit {
                code: Some(0),
                result: Some("success".to_owned()),
                signal: None,
            },
            Some("report"),
            None,
            &Default::default(),
        );
        self.spawn(fragment);
    }

    /// The run names `locator`; `start` bytes belong to earlier lives.
    fn conversation(&mut self, run: Id, locator: &str, start: Option<u64>, end: Option<u64>) {
        let mut fragment = Fragment::empty();
        let handle = fragment.put::<blobencodings::UTF8String, _>(locator.to_owned());
        fragment += entity! { ExclusiveId::force_ref(&run) @
            archive_schema::source_projection::source_locator: handle,
            attrs::archive_start?: start,
            attrs::archive_end?: end,
        };
        self.spawn(fragment);
    }

    fn imported(&mut self, run: Id, bytes: u64) {
        self.spawn(
            entity! { ExclusiveId::force_ref(&run) @ attrs::archive_imported: bytes }.into(),
        );
    }

    fn claude_session(&mut self, text: &str) {
        let mut all = Fragment::empty();
        crate::archive_claude_code::project_bytes(
            "/fixture/session.jsonl",
            Bytes::from_source(text.as_bytes().to_vec()),
            |projected| {
                all += projected.fragment;
                Ok(())
            },
        )
        .unwrap();
        self.archived(all);
    }

    fn agy_conversation(&mut self, conversation: &str, text: &str) {
        let mut all = Fragment::empty();
        let path = format!(
            "/home/z/{}/{conversation}/{}",
            crate::archive_agy::BRAIN_PATH,
            crate::archive_agy::TRANSCRIPT_PATH
        );
        crate::archive_agy::project_bytes(
            &path,
            Bytes::from_source(text.as_bytes().to_vec()),
            |projected| {
                all += projected.fragment;
                Ok(())
            },
        )
        .unwrap();
        self.archived(all);
    }

    fn life(&mut self, run: Id, archived: bool) -> Life {
        let spawn = archive(&self.spawn);
        let facts = archive(&self.archive);
        let reader = self.blobs.reader().unwrap();
        let routed = if archived {
            Ok(&facts)
        } else {
            Err("route missing from this workspace")
        };
        super::life(&spawn, &reader, routed, run).unwrap()
    }

    fn turns(&mut self, nodes: &[Id]) -> Vec<Turn> {
        let facts = archive(&self.archive);
        let reader = self.blobs.reader().unwrap();
        super::turns(&facts, &reader, nodes, &BTreeSet::new())
    }
}

const PROMPT: &str = "Wake text, then the brief: check the synthetic track.";

fn session(id: &str, prompt: &str) -> String {
    let prompt = serde_json::to_string(prompt).unwrap();
    format!(
        r#"{{"type":"user","sessionId":"{id}","uuid":"u1","parentUuid":null,"timestamp":"2026-11-26T10:00:00Z","message":{{"role":"user","content":{prompt}}}}}
{{"type":"assistant","sessionId":"{id}","uuid":"a1","parentUuid":"u1","timestamp":"2026-11-26T10:00:02Z","message":{{"role":"assistant","model":"claude-opus-5-5","content":[{{"type":"text","text":"Listening first."}},{{"type":"tool_use","id":"toolu_1","name":"Bash","input":{{"command":"ls"}}}}]}}}}
{{"type":"user","sessionId":"{id}","uuid":"u2","parentUuid":"a1","timestamp":"2026-11-26T10:00:03Z","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"toolu_1","content":[{{"type":"text","text":"track-two.wav"}}]}}]}}}}
{{"type":"assistant","sessionId":"{id}","uuid":"a2","parentUuid":"u2","timestamp":"2026-11-26T10:00:05Z","message":{{"role":"assistant","model":"claude-opus-5-5","content":[{{"type":"text","text":"Track two is there."}}]}}}}
"#
    )
}

#[test]
fn a_claude_run_reads_its_own_session_in_causal_order_with_calls_and_results_linked() {
    let mut fixture = Fixture::default();
    let run = fixture.run("Check the track", PROMPT, "claude", 1);
    fixture.conversation(run, "session/s-1", None, None);
    fixture.settle(run, 9);
    fixture.claude_session(&session("s-1", PROMPT));
    // Another zooid's session in the same Archive stays out.
    fixture.claude_session(&session("s-other", "someone else's brief"));

    let life = fixture.life(run, true);
    assert_eq!(life.opening.len(), 1);
    let History::Own { nodes, basis } = &life.history else {
        panic!("{:?}", life.history)
    };
    assert!(basis.contains("session/s-1"), "{basis}");
    let turns = fixture.turns(nodes);
    let shown: Vec<(Direction, &str, String)> = turns
        .iter()
        .flat_map(|turn| {
            turn.parts.iter().map(move |part| {
                let text = match &part.body {
                    Body::Text(text) => text.clone(),
                    other => format!("{other:?}"),
                };
                (turn.direction, part.kind.as_str(), text)
            })
        })
        .filter(|(_, kind, _)| *kind != "event")
        .collect();
    let kinds: Vec<&str> = shown.iter().map(|(_, kind, _)| *kind).collect();
    assert_eq!(kinds, ["text", "text", "tool-call", "tool-result", "text"]);
    assert_eq!(shown[0], (Direction::Received, "text", PROMPT.to_owned()));
    assert_eq!(shown[1].0, Direction::Produced);
    assert!(shown[2].2.contains("Bash"), "{shown:?}");
    assert_eq!(shown[3].2, "track-two.wav");
    assert!(!shown
        .iter()
        .any(|(_, _, text)| text.contains("someone else")));
    // The result names the node of the call it answers, which comes before it.
    let call = turns
        .iter()
        .position(|turn| turn.parts.iter().any(|part| part.kind == "tool-call"))
        .unwrap();
    let result = turns
        .iter()
        .position(|turn| turn.parts.iter().any(|part| part.kind == "tool-result"))
        .unwrap();
    assert_eq!(turns[result].answers, [turns[call].node]);
    assert!(call < result);
    assert!(turns
        .iter()
        .any(|turn| turn.roles.contains(&"assistant".to_owned())));
    // The archived first input is exactly the prepared text.
    assert_eq!(opening_agrees(&life.opening, &turns), Some(true));
}

#[test]
fn a_huge_prepared_opening_is_kept_whole_and_a_cut_received_input_stays_beside_it() {
    let mut fixture = Fixture::default();
    let prompt = "wake ".repeat(200_000) + "# Brief\nthe end of the brief";
    assert!(prompt.len() > 933_083);
    let run = fixture.run("A long wake", &prompt, "claude", 1);
    fixture.conversation(run, "session/s-cut", None, None);
    fixture.settle(run, 9);
    let received = format!("{}<truncated 741501 bytes>", &prompt[..191_985]);
    fixture.claude_session(&session("s-cut", &received));

    let life = fixture.life(run, true);
    let reader = fixture.blobs.reader().unwrap();
    let [opening] = life.opening.as_slice() else {
        panic!("one opening")
    };
    let whole = reader.get::<View<str>, _>(*opening).unwrap();
    assert_eq!(
        whole.as_ref(),
        prompt.as_str(),
        "the prepared input is kept whole"
    );
    let History::Own { nodes, .. } = &life.history else {
        panic!("{:?}", life.history)
    };
    let turns = fixture.turns(nodes);
    assert_eq!(opening_agrees(&life.opening, &turns), Some(false));
    assert_eq!(
        turns[0].parts[0].body,
        Body::Text(received),
        "what the provider recorded is shown as it is, not replaced by the prepared text"
    );
}

#[test]
fn an_agy_conversation_two_lives_share_is_never_handed_whole_to_either() {
    let mut fixture = Fixture::default();
    let first = fixture.run("Resident", "first wake", "agy", 1);
    fixture.conversation(first, "session/c-1", None, Some(120));
    fixture.settle(first, 5);
    fixture.imported(first, 120);
    let second = fixture.run("Resident", "second wake", "agy", 10);
    fixture.conversation(second, "session/c-1", Some(120), Some(260));
    fixture.settle(second, 20);
    fixture.imported(second, 260);
    let transcript = [
        r#"{"source":"USER_INPUT","content":"first wake","step_index":1}"#,
        r#"{"source":"MODEL","type":"PLANNER_RESPONSE","content":"first life speaking","step_index":2}"#,
        r#"{"source":"USER_INPUT","content":"second wake","step_index":3}"#,
        r#"{"source":"MODEL","type":"PLANNER_RESPONSE","content":"second life speaking","step_index":4}"#,
    ]
    .join("\n")
        + "\n";
    fixture.agy_conversation("c-1", &transcript);

    for run in [first, second] {
        let life = fixture.life(run, true);
        let History::Shared { locator, runs, why } = &life.history else {
            panic!("{:?}", life.history)
        };
        assert_eq!(locator, "session/c-1");
        assert_eq!(runs, &[first, second], "oldest first, this one included");
        assert!(why.contains("bytes"), "{why}");
    }
    // A fresh first life alone (no later run seen) would be its own; a
    // continuing life alone is still shared with its earlier lives.
    let mut alone = Fixture::default();
    let continuing = alone.run("Resident", "second wake", "agy", 10);
    alone.conversation(continuing, "session/c-1", Some(120), None);
    assert!(matches!(
        alone.life(continuing, true).history,
        History::Shared { .. }
    ));
    // The whole conversation stays readable as the conversation.
    let facts = archive(&fixture.archive);
    let whole = conversation(&facts, "session/c-1").unwrap().unwrap();
    let texts: Vec<String> = fixture
        .turns(&whole)
        .into_iter()
        .flat_map(|turn| turn.parts)
        .filter_map(|part| match part.body {
            Body::Text(text) => Some(text),
            _ => None,
        })
        .collect();
    assert_eq!(
        texts,
        [
            "first wake",
            "first life speaking",
            "second wake",
            "second life speaking"
        ]
    );
}

#[test]
fn missing_routes_unimported_and_known_empty_histories_are_different_answers() {
    let mut fixture = Fixture::default();
    let running = fixture.run("Running", "p", "claude", 1);
    fixture.conversation(running, "session/s-run", None, None);
    let lost = fixture.run("Lost", "p", "claude", 2);
    fixture.conversation(lost, "session/s-lost", None, None);
    fixture.settle(lost, 3);
    fixture.imported(lost, 4096);
    let empty = fixture.run("Empty", "p", "agy", 3);
    fixture.settle(empty, 4);
    fixture.imported(empty, 0);
    let unlinked = fixture.run("Codex", "p", "codex", 4);
    fixture.settle(unlinked, 5);
    let fresh_local = fixture.run("Local", "p", "local", 5);
    let unprompted = Id::from_hex("00112233445566778899AABBCCDDEEFF").unwrap();

    assert!(
        matches!(fixture.life(running, false).history, History::Unavailable(ref why) if why.contains("route missing"))
    );
    assert!(
        matches!(fixture.life(running, true).history, History::NotArchived(ref why) if why.contains("has not ended"))
    );
    assert!(
        matches!(fixture.life(lost, true).history, History::NotArchived(ref why) if why.contains("4096 bytes"))
    );
    assert!(matches!(
        fixture.life(empty, true).history,
        History::Empty(_)
    ));
    assert!(matches!(
        fixture.life(unlinked, true).history,
        History::Unlinked(_)
    ));
    assert!(
        matches!(fixture.life(fresh_local, true).history, History::Unlinked(ref why) if why.contains("fresh local run"))
    );
    assert!(fixture.life(unprompted, true).opening.is_empty());
}

#[test]
fn a_resumed_local_run_lives_exactly_the_context_that_continues_its_node() {
    let mut fixture = Fixture::default();
    let tail = *rngid();
    let run = fixture.run_resuming("Makap", "p", "local", 1, Some(tail));
    let name = archive(&fixture.spawn);
    let name: TextHandle = find!(
        name: TextHandle,
        pattern!(&name, [{ run @ attrs::of: _?zooid }, { _?zooid @ attrs::name: ?name }])
    )
    .next()
    .unwrap();
    // Two contexts of the same name: one continues the run's node, one
    // continues another.
    let mut nodes = Vec::new();
    for (continues, text) in [(tail, "this run's turn"), (*rngid(), "another life's turn")] {
        let ctx = *rngid();
        let mut fragment = Fragment::empty();
        let fact = crate::blockdag::text_fact(
            archive_schema::content_fact::modality::TEXT,
            archive_schema::content_fact::direction::OUT,
            text.to_owned(),
        )
        .unwrap();
        let (block, _) = crate::blockdag::block_parts(None, None, fact).unwrap();
        let node = block.root().unwrap();
        let witness = crate::blockdag::block_source_projection(
            archive_schema::source_projection::SOURCE_DRIVE,
            block,
        )
        .unwrap();
        let witness_id = witness.root().unwrap();
        fragment += witness;
        fragment += entity! { _ @ occurrence::of: witness_id, context::in_context: ctx };
        fragment += entity! { ExclusiveId::force_ref(&ctx) @
            context::continues: continues,
            context::zooid: name,
        };
        fixture.archived(fragment);
        nodes.push(node);
    }
    let life = fixture.life(run, true);
    let History::Own { nodes: own, basis } = &life.history else {
        panic!("{:?}", life.history)
    };
    assert_eq!(own, &[nodes[0]]);
    assert!(basis.contains("Drive context"), "{basis}");
}
