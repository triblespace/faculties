//! Wrong importer outputs the gate must catch: the round-2 adversarial
//! review's 25 cases, kept as a permanent test, and the round-3 cases for
//! report vocabulary (G8) and gap items (G9). Synthetic transcripts only.
//!
//! Each case imports a right transcript with the Claude Code importer, then
//! breaks the fragment (or the source) in one way an importer could, and
//! asks the gate. A case passes when the gate fails the unit; the report
//! cases also check that no value reaches a printed row.

use serde_json::{json, Value};
use triblespace::core::blob::MemoryBlobStoreSnapshot;
use triblespace::core::inline::encodings::UnknownInline;
use triblespace::prelude::blobencodings::UTF8String;
use triblespace::prelude::inlineencodings::{GenId, Handle};
use triblespace::prelude::*;

use crate::archive_v3 as av3;
use crate::archive_v3::claude_code::import;
use crate::archive_v3::fields::claude_code::TABLE;
use crate::archive_v3::gate::{self, Report};
use crate::schemas::archive_v3 as v3;

/// An attribute no schema names (minted with `trible genid` for this test).
const UNKNOWN_ATTR: Id = triblespace::macros::id_hex!("089860CF2C06CB752BB24731E1D07142");

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

fn stamp(second: u32) -> String {
    format!("2026-03-01T10:00:{second:02}.000Z")
}

fn user(uuid: &str, parent: Option<&str>, second: u32, content: Value) -> Value {
    json!({
        "type": "user", "sessionId": "s-1", "uuid": uuid, "parentUuid": parent,
        "timestamp": stamp(second), "cwd": "/synthetic", "version": "0.0.0",
        "isSidechain": false, "userType": "external",
        "message": {"role": "user", "content": content}
    })
}

fn assistant(uuid: &str, parent: &str, second: u32, id: &str, block: Value, stop: Value) -> Value {
    json!({
        "type": "assistant", "sessionId": "s-1", "uuid": uuid, "parentUuid": parent,
        "timestamp": stamp(second), "requestId": format!("req-{id}"),
        "message": {"id": id, "type": "message", "role": "assistant",
            "model": "claude-opus-4-6", "content": [block],
            "stop_reason": stop, "stop_sequence": null, "usage": {"input_tokens": 1}}
    })
}

fn said(text: &str) -> Value {
    json!({"type": "text", "text": text})
}

fn call(id: &str, name: &str, input: Value) -> Value {
    json!({"type": "tool_use", "id": id, "name": name, "input": input, "caller": {"type": "direct"}})
}

fn result(call: &str, content: Value) -> Value {
    json!([{"type": "tool_result", "tool_use_id": call, "content": content, "is_error": false}])
}

fn import_ok(source: &str) -> Imported {
    let mut fragment = import(source.as_bytes()).unwrap_or_else(|failure| panic!("{failure}"));
    let blobs = fragment.blobs_mut().snapshot().unwrap();
    Imported { fragment, blobs }
}

fn text_of(imported: &Imported, handle: Inline<Handle<UTF8String>>) -> String {
    use anybytes::View;
    use triblespace::core::repo::BlobStoreGet;
    imported
        .blobs
        .get::<View<str>, UTF8String>(handle)
        .unwrap()
        .as_ref()
        .to_owned()
}

/// The nodes that hold a fact with this payload.
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

/// Two prompts on one line, a branch under the first answer, a call and its
/// result, one message over two records, and a title.
fn source() -> String {
    let title = json!({"type": "ai-title", "sessionId": "s-1", "aiTitle": "A synthetic chat"});
    jsonl(&[
        user("u1", None, 0, json!("Hi.")),
        assistant("a1", "u1", 1, "m1", said("Hello."), json!("end_turn")),
        user("u2", Some("a1"), 2, json!("Again.")),
        assistant("a2", "u2", 3, "m2", said("Hello."), json!("end_turn")),
        user("u3", Some("a1"), 4, json!("Branch.")),
        assistant(
            "a3",
            "u3",
            5,
            "m3",
            call("t1", "Bash", json!({"command": "ls"})),
            json!("tool_use"),
        ),
        user("u4", Some("a3"), 6, result("t1", json!("out"))),
        assistant("a4", "u4", 7, "m4", said("Done."), json!(null)),
        assistant("a5", "a4", 8, "m4", said("Done."), json!("end_turn")),
        title,
    ])
}

fn of(set: &TribleSet, e: Id, a: Id) -> Vec<Trible> {
    set.iter()
        .filter(|t| *t.e() == e && *t.a() == a)
        .copied()
        .collect()
}

fn ids(set: &TribleSet, e: Id, a: Id) -> Vec<Id> {
    of(set, e, a)
        .iter()
        .filter_map(|t| t.v::<GenId>().try_from_inline::<Id>().ok())
        .collect()
}

fn subjects_with(set: &TribleSet, a: Id, v: Id) -> Vec<Id> {
    set.iter()
        .filter(|t| *t.a() == a && t.v::<GenId>().try_from_inline::<Id>().ok() == Some(v))
        .map(|t| *t.e())
        .collect()
}

fn remove(f: &mut Fragment, tribles: &[Trible]) {
    let mut gone = TribleSet::new();
    for t in tribles {
        gone.insert(t);
    }
    let kept = f.facts().difference(&gone);
    *f.facts_mut() = kept;
}

fn add(f: &mut Fragment, t: Trible) {
    f.facts_mut().insert(&t);
}

fn gv(id: Id) -> Inline<GenId> {
    id.to_inline()
}

fn set_id(f: &mut Fragment, e: Id, a: Id, v: Id) {
    let old = of(f.facts(), e, a);
    remove(f, &old);
    add(f, Trible::force(&e, &a, &gv(v)));
}

fn entity(set: &TribleSet, e: Id) -> Vec<Trible> {
    set.iter().filter(|t| *t.e() == e).copied().collect()
}

fn witness_of(f: &Fragment, node: Id) -> Vec<Id> {
    subjects_with(f.facts(), v3::witness::projects_to.id(), node)
}

fn occ_of(f: &Fragment, w: Id) -> Vec<Id> {
    subjects_with(f.facts(), v3::occurrence::of.id(), w)
}

fn holds(f: &Fragment, node: Id) -> Vec<Id> {
    ids(f.facts(), node, v3::node::holds.id())
}

fn prev(f: &Fragment, node: Id) -> Vec<Id> {
    ids(f.facts(), node, v3::node::previous.id())
}

fn next(f: &Fragment, node: Id) -> Vec<Id> {
    subjects_with(f.facts(), v3::node::previous.id(), node)
}

fn witnessed(imported: &Imported, text: &str) -> Vec<Id> {
    nodes_saying(imported, text)
        .into_iter()
        .filter(|n| !witness_of(&imported.fragment, *n).is_empty())
        .collect()
}

fn one(imported: &Imported, text: &str) -> Id {
    let n = witnessed(imported, text);
    assert_eq!(n.len(), 1, "{text}");
    n[0]
}

fn run(source: &str, f: Fragment) -> Report {
    let mut f = f;
    let blobs = f.blobs_mut().snapshot().unwrap();
    gate::check(&TABLE, source.as_bytes(), f.facts(), &blobs)
}

/// A new fact and node after `after`, witnessed (with the claims of
/// `claims_from`'s occurrence copied onto a new occurrence) or not.
fn append(f: &mut Fragment, after: Id, fact: Fragment, witnessed: Option<Id>) -> Id {
    let node = av3::node([after], fact).unwrap();
    let node_id = node.root().unwrap();
    *f += node;
    if let Some(claims_from) = witnessed {
        let w = av3::witness(
            ids(
                f.facts(),
                witness_of(f, claims_from)[0],
                v3::witness::source_namespace.id(),
            )[0],
            node_id,
        )
        .unwrap();
        let w_id = w.root().unwrap();
        *f += w;
        let src_occ = occ_of(f, witness_of(f, claims_from)[0])[0];
        let fresh = Id::new([0x5A; 16]).unwrap();
        for t in entity(f.facts(), src_occ) {
            if *t.a() == v3::occurrence::of.id() {
                add(f, Trible::force(&fresh, t.a(), &gv(w_id)));
            } else {
                add(f, Trible::force(&fresh, t.a(), t.v::<UnknownInline>()));
            }
        }
    }
    node_id
}

fn text_fact(direction: Id, text: &str) -> Fragment {
    av3::fact(av3::FactCore {
        modality: Some(v3::fact::modality::TEXT),
        direction: Some(direction),
        payload: Some(text.to_owned()),
        ..Default::default()
    })
    .unwrap()
}

/// The witnessed "Done." whose predecessor is the other "Done.".
fn second_done(imported: &Imported) -> Id {
    let done = witnessed(imported, "Done.");
    done.iter()
        .copied()
        .find(|n| {
            prev(&imported.fragment, *n)
                .first()
                .is_some_and(|p| done.contains(p))
        })
        .unwrap()
}

/// The witnessed "Hello." that answers "Again.".
fn second_hello(imported: &Imported) -> Id {
    let again = one(imported, "Again.");
    witnessed(imported, "Hello.")
        .into_iter()
        .find(|n| prev(&imported.fragment, *n).first() == Some(&again))
        .unwrap()
}

/// The values a printed report must never hold.
const SECRETS: [&str; 4] = ["hunter2", "3f2a9c1e", "Bremen", "s-1"];

fn leaks(text: &str) -> Vec<&'static str> {
    SECRETS.into_iter().filter(|v| text.contains(v)).collect()
}

/// Every case, by name, with the report the gate gave.
fn cases() -> Vec<(&'static str, Report)> {
    let s = source();
    let fresh = || import_ok(&s);
    let mut out: Vec<(&'static str, Report)> = Vec::new();

    // ---- M1: invented out text
    {
        let i = fresh();
        let a2 = one(&i, "Again.");
        let answer = next(&i.fragment, a2)[0];
        let mut f = i.fragment;
        let inv = text_fact(v3::fact::direction::OUT, "Invented.");
        let inv_id = inv.root().unwrap();
        f += inv;
        add(
            &mut f,
            Trible::force(&answer, &v3::node::holds.id(), &gv(inv_id)),
        );
        out.push((
            "M1a extra invented fact held by a witnessed node",
            run(&s, f),
        ));
    }
    {
        let i = fresh();
        let leaf = second_done(&i);
        let mut f = i.fragment;
        append(
            &mut f,
            leaf,
            text_fact(v3::fact::direction::OUT, "Invented."),
            None,
        );
        out.push((
            "M1b unwitnessed node with invented out text after leaf",
            run(&s, f),
        ));
    }
    {
        let i = fresh();
        let leaf = second_done(&i);
        let mut f = i.fragment;
        append(
            &mut f,
            leaf,
            text_fact(v3::fact::direction::OUT, "Invented."),
            Some(leaf),
        );
        out.push((
            "M1c witnessed node with invented out text after leaf",
            run(&s, f),
        ));
    }
    {
        let i = fresh();
        let leaf = second_done(&i);
        let mut f = i.fragment;
        // Re-uses the existing "Hello." out fact: every token exists in B.
        append(
            &mut f,
            leaf,
            text_fact(v3::fact::direction::OUT, "Hello."),
            Some(leaf),
        );
        out.push((
            "M1d witnessed copy of an existing out text appended",
            run(&s, f),
        ));
    }
    {
        let i = fresh();
        let a1 = witnessed(&i, "Hello.");
        let mut f = i.fragment;
        let fact = holds(&f, a1[0])[0];
        let old = of(f.facts(), fact, v3::fact::payload.id());
        remove(&mut f, &old);
        let h: Inline<Handle<UTF8String>> = f.put::<UTF8String, _>("Hellp.".to_owned());
        add(&mut f, Trible::force(&fact, &v3::fact::payload.id(), &h));
        out.push(("M1e out text changed in place", run(&s, f)));
    }

    // ---- M2: dropped duplicates
    {
        let i = fresh();
        let leaf = second_done(&i);
        let mut f = i.fragment;
        let first = prev(&f, leaf)[0];
        // The second "Done." collapsed onto the first: its witness projects
        // to the first node, its node is gone, its end moves along.
        let w = witness_of(&f, leaf)[0];
        set_id(&mut f, w, v3::witness::projects_to.id(), first);
        let gone = entity(f.facts(), leaf);
        remove(&mut f, &gone);
        add(
            &mut f,
            Trible::force(&first, &v3::node::ends.id(), &gv(v3::node::ends::END_TURN)),
        );
        out.push((
            "M2a consecutive duplicate collapsed onto one node",
            run(&s, f),
        ));
    }
    {
        let i = fresh();
        let leaf = second_done(&i);
        let mut f = i.fragment;
        let w = witness_of(&f, leaf)[0];
        let mut gone = entity(f.facts(), leaf);
        gone.extend(entity(f.facts(), w));
        for o in occ_of(&f, w) {
            gone.extend(entity(f.facts(), o));
        }
        remove(&mut f, &gone);
        out.push((
            "M2b consecutive duplicate dropped with its witness",
            run(&s, f),
        ));
    }
    {
        let i = fresh();
        let a2 = second_hello(&i);
        let a1 = witnessed(&i, "Hello.")
            .into_iter()
            .find(|n| *n != a2)
            .unwrap();
        let mut f = i.fragment;
        let w = witness_of(&f, a2)[0];
        set_id(&mut f, w, v3::witness::projects_to.id(), a1);
        let gone = entity(f.facts(), a2);
        remove(&mut f, &gone);
        out.push((
            "M2c repeated text in another window merged onto the first",
            run(&s, f),
        ));
    }

    // ---- M3: swapped order
    {
        let i = fresh();
        let again = one(&i, "Again.");
        let a2 = second_hello(&i);
        let mut f = i.fragment;
        let fa = holds(&f, again)[0];
        let fb = holds(&f, a2)[0];
        set_id(&mut f, again, v3::node::holds.id(), fb);
        set_id(&mut f, a2, v3::node::holds.id(), fa);
        out.push(("M3 swapped two consecutive items", run(&s, f)));
    }

    // ---- M4: wrong branch
    {
        let i = fresh();
        let branch = one(&i, "Branch.");
        let a2 = second_hello(&i);
        let mut f = i.fragment;
        set_id(&mut f, branch, v3::node::previous.id(), a2);
        out.push(("M4 a branch grafted under the other branch", run(&s, f)));
    }

    // ---- M5: correlators and pairing
    {
        let i = fresh();
        let mutated = s.replacen(
            "\"tool_use_id\":\"t1\"",
            "\"tool_use_id\":{\"id\":\"t1\",\"zz\":1}",
            1,
        );
        assert_ne!(mutated, s);
        out.push((
            "M5a unknown member inside a correlator (source side)",
            run(&mutated, i.fragment),
        ));
    }
    {
        let i = fresh();
        let mutated = s.replacen("\"tool_use_id\":\"t1\"", "\"tool_use_id\":\"t9\"", 1);
        assert_ne!(mutated, s);
        out.push((
            "M5b result correlator names another call (pairing differs)",
            run(&mutated, i.fragment),
        ));
    }
    {
        let i = fresh();
        let again = one(&i, "Again.");
        let mut f = i.fragment;
        let link: Vec<Trible> = f
            .facts()
            .iter()
            .filter(|t| *t.a() == v3::node::responds_to.id())
            .copied()
            .collect();
        assert_eq!(link.len(), 1, "the fixture pairs one result");
        let target = link[0].v::<GenId>().try_from_inline::<Id>().unwrap();
        remove(&mut f, &link);
        add(
            &mut f,
            Trible::force(&again, &v3::node::responds_to.id(), &gv(target)),
        );
        out.push(("M5c responds_to moved to the wrong node", run(&s, f)));
    }

    // ---- M6: stray and misplaced annotations
    {
        let i = fresh();
        let again = one(&i, "Again.");
        let mut f = i.fragment;
        add(
            &mut f,
            Trible::force(&again, &v3::node::ends.id(), &gv(v3::node::ends::END_TURN)),
        );
        out.push((
            "M6a ends:end_turn (a value present elsewhere) on a user node",
            run(&s, f),
        ));
    }
    {
        let i = fresh();
        let again = one(&i, "Again.");
        let mut f = i.fragment;
        add(
            &mut f,
            Trible::force(&again, &v3::node::ends.id(), &gv(v3::node::ends::REFUSAL)),
        );
        out.push((
            "M6b ends:refusal (a value in no source) on a node",
            run(&s, f),
        ));
    }
    {
        let i = fresh();
        let hi = one(&i, "Hi.");
        let mut f = i.fragment;
        let g = subjects_with(
            f.facts(),
            v3::gloss::category.id(),
            v3::gloss::category::TITLE,
        )[0];
        set_id(&mut f, g, v3::gloss::describes.id(), hi);
        out.push(("M6c title gloss moved to another node", run(&s, f)));
    }
    {
        let i = fresh();
        let hi = one(&i, "Hi.");
        let again = one(&i, "Again.");
        let mut f = i.fragment;
        let o1 = occ_of(&f, witness_of(&f, hi)[0])[0];
        let o2 = occ_of(&f, witness_of(&f, again)[0])[0];
        let t1 = of(f.facts(), o1, v3::witness::timestamp.id());
        let t2 = of(f.facts(), o2, v3::witness::timestamp.id());
        remove(&mut f, &t1);
        remove(&mut f, &t2);
        for t in &t1 {
            add(&mut f, Trible::force(&o2, t.a(), t.v::<UnknownInline>()));
        }
        for t in &t2 {
            add(&mut f, Trible::force(&o1, t.a(), t.v::<UnknownInline>()));
        }
        out.push((
            "M6d timestamps swapped between two records' occurrences",
            run(&s, f),
        ));
    }
    {
        let i = fresh();
        let hi = one(&i, "Hi.");
        let again = one(&i, "Again.");
        let mut f = i.fragment;
        let w1 = witness_of(&f, hi)[0];
        let w2 = witness_of(&f, again)[0];
        let o1 = occ_of(&f, w1)[0];
        let o2 = occ_of(&f, w2)[0];
        set_id(&mut f, o1, v3::occurrence::of.id(), w2);
        set_id(&mut f, o2, v3::occurrence::of.id(), w1);
        out.push((
            "M6e whole occurrences swapped between witnesses",
            run(&s, f),
        ));
    }
    {
        let i = fresh();
        let again = one(&i, "Again.");
        let mut f = i.fragment;
        add(&mut f, Trible::force(&again, &UNKNOWN_ATTR, &gv(again)));
        out.push(("M6f unknown attribute on a node", run(&s, f)));
    }
    {
        let i = fresh();
        let again = one(&i, "Again.");
        let mut f = i.fragment;
        add(
            &mut f,
            Trible::force(
                &again,
                &v3::node::rendering.id(),
                &gv(v3::node::rendering::UNRECORDED),
            ),
        );
        out.push(("M6g rendering:unrecorded on a node", run(&s, f)));
    }
    {
        let i = fresh();
        let again = one(&i, "Again.");
        let hi = one(&i, "Hi.");
        let mut f = i.fragment;
        add(
            &mut f,
            Trible::force(&again, &v3::node::recalls.id(), &gv(hi)),
        );
        out.push(("M6h stray recalls link", run(&s, f)));
    }
    {
        let i = fresh();
        let mut f = i.fragment;
        let stray = Id::new([0x77; 16]).unwrap();
        add(&mut f, Trible::force(&stray, &UNKNOWN_ATTR, &gv(stray)));
        out.push(("M6i stray disconnected entity", run(&s, f)));
    }
    {
        let i = fresh();
        let again = one(&i, "Again.");
        let mut f = i.fragment;
        f += av3::gloss(again, v3::gloss::category::TITLE, "A synthetic chat").unwrap();
        out.push(("M6j duplicate title gloss on a second node", run(&s, f)));
    }

    // ---- M8: changed direction
    {
        let i = fresh();
        let branch = one(&i, "Branch.");
        let mut f = i.fragment;
        let fact = holds(&f, branch)[0];
        set_id(
            &mut f,
            fact,
            v3::fact::direction.id(),
            v3::fact::direction::OUT,
        );
        out.push(("M8 in text flipped to out", run(&s, f)));
    }

    // ---- M7: unknown keys whose names are values
    {
        let mut a1: Value = serde_json::from_str(s.lines().nth(1).unwrap()).unwrap();
        a1["hunter2.diary"] = json!(1);
        a1["3f2a9c1e-77aa-4b2c-9d0e-123456789abc"] = json!(1);
        a1["message"]["content"][0]["Bremen-secret"] = json!("x");
        let mut lines: Vec<String> = s.lines().map(str::to_owned).collect();
        lines[1] = serde_json::to_string(&a1).unwrap();
        let mutated = lines.join("\n") + "\n";
        out.push(("M7 unknown keys", run(&mutated, import_ok(&s).fragment)));
    }
    out
}

#[test]
fn the_gate_catches_every_adversarial_output() {
    let s = source();
    let baseline = import_ok(&s);
    let report = run(&s, baseline.fragment);
    assert!(report.passed(), "baseline: {report}");

    let cases = cases();
    assert_eq!(cases.len(), 25);
    let mut missed = Vec::new();
    for (name, report) in &cases {
        if report.passed() {
            println!("MISSED\t{name}");
            missed.push(*name);
        } else {
            let mut kinds: Vec<String> = report.rows.iter().map(|r| r.kind.to_string()).collect();
            kinds.sort();
            kinds.dedup();
            println!("caught\t{name}\t[{}]", kinds.join(","));
        }
        let text = format!("{report}");
        assert!(leaks(&text).is_empty(), "{name} printed a value:\n{text}");
    }
    assert!(
        missed.is_empty(),
        "the gate missed {} of 25: {missed:?}",
        missed.len()
    );
}

// ---------------------------------------------------------------------------
// G8: reports print vocabulary only.

#[test]
fn g8_a_key_the_table_does_not_name_prints_as_key() {
    let s = source();
    let mut a1: Value = serde_json::from_str(s.lines().nth(1).unwrap()).unwrap();
    a1["hunter2.diary"] = json!(1);
    a1["message"]["content"][0]["Bremen-secret"] = json!("x");
    let mut lines: Vec<String> = s.lines().map(str::to_owned).collect();
    lines[1] = serde_json::to_string(&a1).unwrap();
    let mutated = lines.join("\n") + "\n";
    let report = run(&mutated, import_ok(&s).fragment);
    let text = format!("{report}");
    assert!(!report.passed());
    assert!(leaks(&text).is_empty(), "{text}");
    assert!(text.contains("<key>"), "{text}");
    // The walker's own rows hold no value either.
    let walked = gate::walk::walk(&TABLE, mutated.as_bytes());
    for (path, _, _) in &walked.rows {
        assert!(leaks(path).is_empty(), "{path}");
    }
    // An importer failure path, as a real-run program prints it.
    let failure = import(mutated.as_bytes())
        .err()
        .expect("the importer refuses");
    let printed = gate::vocabulary(&TABLE, &failure.path);
    assert!(leaks(&printed).is_empty(), "{printed}");
}

#[test]
fn g8_a_discriminator_value_outside_the_vocabulary_prints_as_value() {
    let s = source();
    let typed = s.replacen("\"type\":\"user\"", "\"type\":\"hunter2pass\"", 1);
    assert_ne!(typed, s);
    let report = run(&typed, import_ok(&s).fragment);
    let text = format!("{report}");
    assert!(!report.passed());
    assert!(!text.contains("hunter2pass"), "{text}");
    assert!(text.contains("{<value>}"), "{text}");
    let failure = import(typed.as_bytes())
        .err()
        .expect("the importer refuses");
    let printed = gate::vocabulary(&TABLE, &failure.path);
    assert!(!printed.contains("hunter2pass"), "{printed}");
}

// ---------------------------------------------------------------------------
// G9: a gap node is an item of its window.

/// The root of the unrecorded section: the header gap node.
fn header_gap(f: &Fragment) -> Id {
    let roots: Vec<Id> = f
        .facts()
        .iter()
        .filter(|t| *t.a() == v3::node::holds.id())
        .map(|t| *t.e())
        .filter(|n| prev(f, *n).is_empty())
        .collect();
    assert_eq!(roots.len(), 1, "one root");
    roots[0]
}

#[test]
fn g9_a_missing_gap_node_is_a_count_fault() {
    let s = source();
    let i = import_ok(&s);
    let mut f = i.fragment;
    // Drop the header gap: the system gap becomes the root.
    let header = header_gap(&f);
    for child in next(&f, header) {
        let link = of(f.facts(), child, v3::node::previous.id());
        remove(&mut f, &link);
    }
    let gone = entity(f.facts(), header);
    remove(&mut f, &gone);
    let report = run(&s, f);
    assert!(!report.passed(), "a missing header gap passed");
    assert!(
        report.rows.iter().any(|row| row.kind == gate::Kind::Count),
        "{report}"
    );
}

#[test]
fn g9_an_extra_gap_node_is_a_count_fault() {
    let s = source();
    let i = import_ok(&s);
    let again = one(&i, "Again.");
    let mut f = i.fragment;
    // A span gap between "Hello." and "Again.".
    let before = prev(&f, again)[0];
    let gap = av3::fact(av3::FactCore {
        absent: Some(v3::fact::absent::SPAN),
        ..Default::default()
    })
    .unwrap();
    let node = av3::node([before], gap).unwrap();
    let node_id = node.root().unwrap();
    f += node;
    set_id(&mut f, again, v3::node::previous.id(), node_id);
    let report = run(&s, f);
    assert!(!report.passed(), "an extra span gap passed");
    assert!(
        report.rows.iter().any(|row| row.kind == gate::Kind::Count),
        "{report}"
    );
}

#[test]
fn g9_a_gap_of_another_kind_is_a_count_fault() {
    let s = source();
    let i = import_ok(&s);
    let mut f = i.fragment;
    // The tool-definition span gap of the unrecorded section, made a body gap.
    let spans = subjects_with(f.facts(), v3::fact::absent.id(), v3::fact::absent::SPAN);
    let tools: Vec<Id> = spans
        .into_iter()
        .filter(|fact| {
            ids(f.facts(), *fact, v3::fact::modality.id())
                == vec![v3::fact::modality::TOOL_DEFINITION]
        })
        .collect();
    assert_eq!(tools.len(), 1);
    set_id(
        &mut f,
        tools[0],
        v3::fact::absent.id(),
        v3::fact::absent::BODY,
    );
    let report = run(&s, f);
    assert!(!report.passed(), "a gap of the wrong kind passed");
}

#[test]
fn g8_vocabulary_keeps_what_the_table_names() {
    for path in [
        "{user}.message.content[]{tool_result}.tool_use_id",
        "{assistant}.message{message}.content[]{text}.text",
        "{attachment}.attachment{queued_command}.prompt[]",
        "<record>",
        "<witness>",
    ] {
        assert_eq!(gate::vocabulary(&TABLE, path), path);
    }
    assert_eq!(
        gate::vocabulary(&TABLE, "{user}.hunter2.diary"),
        "{user}.<key>.<key>"
    );
    assert_eq!(
        gate::vocabulary(&TABLE, "{hunter2pass}.message"),
        "{<value>}.<key>"
    );
    assert_eq!(
        gate::vocabulary(&TABLE, "{user}.3f2a9c1e-77aa-4b2c-9d0e-123456789abc"),
        "{user}.<id>"
    );
    assert_eq!(
        gate::file_path(
            "/synthetic/rollout-2026-07-12T10-11-12-3f2a9c1e-77aa-4b2c-9d0e-123456789abc.jsonl"
        ),
        "/synthetic/rollout-<id>T<id>.jsonl"
    );
}
