//! Wrong importer outputs the gate must catch, kept as permanent tests:
//! the round-2 adversarial review's 25 cases, the round-3 review's 45
//! (Claude Code and Codex, plus the importer refusal of a message whose
//! content is a string), and the round-3 cases for report vocabulary (G8)
//! and gap items (G9), and the round-4 review's 50 attacks with its three
//! controls (G17). Synthetic transcripts only.
//!
//! Each case imports a right transcript, then breaks the fragment (or the
//! source) in one way an importer could, and asks the gate. A case passes
//! when the gate fails the unit (or the importer refuses it); the report
//! cases also check that no value reaches a printed row.

use serde_json::{json, Value};
use triblespace::core::blob::MemoryBlobStoreSnapshot;
use triblespace::core::inline::encodings::UnknownInline;
use triblespace::core::metadata;
use triblespace::prelude::blobencodings::{RawBytes, UTF8String};
use triblespace::prelude::inlineencodings::{GenId, Handle};
use triblespace::prelude::*;

use crate::archive_v3 as av3;
use crate::archive_v3::claude_code::{import, Failure};
use crate::archive_v3::fields::claude_code::TABLE;
use crate::archive_v3::fields::codex::TABLE as CX;
use crate::archive_v3::fields::FieldTable;
use crate::archive_v3::gate::{self, Report};
use crate::schemas::archive_v3 as v3;
use crate::schemas::blockdag::source_projection::{SOURCE_CHATGPT, SOURCE_CODEX};

/// An attribute no schema names (minted with `trible genid` for this test).
const UNKNOWN_ATTR: Id = triblespace::macros::id_hex!("089860CF2C06CB752BB24731E1D07142");
/// Entity ids the cases give the entities they make up, each minted with
/// `trible genid`.
const FRESH_OCCURRENCE: Id = triblespace::macros::id_hex!("EAAEEAEB4081B9C6FD3B6357708B5ED8");
const STRAY: Id = triblespace::macros::id_hex!("3C14CEC0370990DD0758EA88404AC815");
const FRESH_A: Id = triblespace::macros::id_hex!("B3DA0A81E752004BBE2CC041A1556A8F");
const FRESH_B: Id = triblespace::macros::id_hex!("A4E98650815A3A269D887718FBE80CD0");
const FRESH_C: Id = triblespace::macros::id_hex!("C61504FDF5828A596ADAA2E1E64E512E");
const FRESH_D: Id = triblespace::macros::id_hex!("80A91A6BCEFF4B381D5AC7E49C5F90BE");
const FRESH_E: Id = triblespace::macros::id_hex!("3F5DF35A2CBAD0FE4208BA0595E48060");

type Importer = fn(&[u8]) -> Result<Fragment, Failure>;

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

fn import_with(import: Importer, source: &str) -> Imported {
    let mut fragment = import(source.as_bytes()).unwrap_or_else(|failure| panic!("{failure}"));
    let blobs = fragment.blobs_mut().snapshot().unwrap();
    Imported { fragment, blobs }
}

/// The `ends` claims of an occurrence.
fn ends_of(f: &Fragment, occurrence: Id) -> Vec<Trible> {
    of(f.facts(), occurrence, v3::witness::ends.id())
}

fn import_ok(source: &str) -> Imported {
    import_with(import, source)
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

fn run_on(table: &FieldTable, source: &str, f: Fragment) -> Report {
    let mut f = f;
    let blobs = f.blobs_mut().snapshot().unwrap();
    gate::check(table, source.as_bytes(), f.facts(), &blobs)
}

fn run(source: &str, f: Fragment) -> Report {
    run_on(&TABLE, source, f)
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
        let fresh = FRESH_OCCURRENCE;
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
        let stray = STRAY;
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

// ---------------------------------------------------------------------------
// Round 3: the review's 45 wrong outputs, aimed at what A reads, the
// placement checks (G6, G7) and Codex.

fn subjects_of(set: &TribleSet, a: Id) -> Vec<(Id, Id)> {
    set.iter()
        .filter(|t| *t.a() == a)
        .filter_map(|t| Some((*t.e(), t.v::<GenId>().try_from_inline::<Id>().ok()?)))
        .collect()
}

fn subjects_any(set: &TribleSet, a: Id) -> Vec<Id> {
    set.iter().filter(|t| *t.a() == a).map(|t| *t.e()).collect()
}

fn unwitnessed(imported: &Imported, text: &str) -> Vec<Id> {
    nodes_saying(imported, text)
        .into_iter()
        .filter(|n| witness_of(&imported.fragment, *n).is_empty())
        .collect()
}

/// A node that holds nothing, spliced between `before` and `after`.
fn splice_empty(f: &mut Fragment, before: Id, after: Id, id: Id) {
    add(f, Trible::force(&id, &v3::node::previous.id(), &gv(before)));
    add(
        f,
        Trible::force(&id, &metadata::tag.id(), &gv(v3::node::KIND)),
    );
    set_id(f, after, v3::node::previous.id(), id);
}

/// A gloss entity with no category, holding text.
fn categoryless_gloss(f: &mut Fragment, node: Id, text: &str, id: Id) {
    let h: Inline<Handle<UTF8String>> = f.put::<UTF8String, _>(text.to_owned());
    add(f, Trible::force(&id, &v3::gloss::describes.id(), &gv(node)));
    add(f, Trible::force(&id, &v3::gloss::text.id(), &h));
    add(
        f,
        Trible::force(&id, &metadata::tag.id(), &gv(v3::gloss::KIND)),
    );
}

/// Every attribute of `from` copied under `to`.
fn clone_entity(f: &mut Fragment, from: Id, to: Id) {
    for t in entity(f.facts(), from) {
        add(f, Trible::force(&to, t.a(), t.v::<UnknownInline>()));
    }
}

/// The gloss of `category` and the node it describes.
fn gloss_on(f: &Fragment, category: Id) -> (Id, Id) {
    let g = subjects_with(f.facts(), v3::gloss::category.id(), category)[0];
    let node = ids(f.facts(), g, v3::gloss::describes.id())[0];
    (g, node)
}

fn thinking(readable: &str) -> Value {
    json!({"type": "thinking", "thinking": readable, "signature": "c2lnbmVkIHRoaW5raW5n"})
}

/// A compaction preserving the old tail; the summary continues it.
fn compaction_source() -> String {
    let boundary = json!({
        "type": "system", "subtype": "compact_boundary", "uuid": "b1", "parentUuid": null,
        "logicalParentUuid": "a2", "sessionId": "s-1", "timestamp": stamp(4),
        "content": "Conversation compacted", "level": "info", "isMeta": false,
        "compactMetadata": {"trigger": "auto", "preTokens": 100,
            "preservedSegment": {"headUuid": "a2", "tailUuid": "a2", "anchorUuid": "c1"},
            "preservedMessages": {"uuids": ["a2"], "allUuids": ["a2"], "anchorUuid": "c1"}}
    });
    let mut summary = user("c1", Some("b1"), 5, json!("Summary of what came before."));
    summary["isCompactSummary"] = json!(true);
    jsonl(&[
        user("u1", None, 0, json!("First.")),
        assistant("a1", "u1", 1, "m1", said("One."), json!("end_turn")),
        user("u2", Some("a1"), 2, json!("Second.")),
        assistant("a2", "u2", 3, "m2", said("Two."), json!("end_turn")),
        boundary,
        summary,
        user("u3", Some("c1"), 6, json!("Third.")),
        assistant("a3", "u3", 7, "m3", said("Three."), json!("end_turn")),
    ])
}

/// S2: the client re-sent her call changed.
fn rewrite_source() -> String {
    let mut a1 = assistant(
        "a1",
        "u1",
        1,
        "m1",
        call("t1", "Bash", json!({"command": "ls"})),
        json!("tool_use"),
    );
    a1["wireToolInputs"] = json!({"t1": {"command": "ls -l"}});
    jsonl(&[
        user("u1", None, 0, json!("Run.")),
        a1,
        user("u2", Some("a1"), 2, result("t1", json!("listing"))),
        assistant("a2", "u2", 3, "m2", said("Listed."), json!("end_turn")),
    ])
}

/// S1: high, high, xhigh.
fn effort_source() -> String {
    let effort = |record: &mut Value, value: &str| {
        record["effort"] = json!(value);
        record["perTurnEffort"] = json!(value);
    };
    let mut a1 = assistant("a1", "u1", 1, "m1", said("One."), json!("end_turn"));
    effort(&mut a1, "high");
    let mut a2 = assistant("a2", "u2", 3, "m2", said("Two."), json!("end_turn"));
    effort(&mut a2, "high");
    let mut a3 = assistant("a3", "u3", 5, "m3", said("Three."), json!("end_turn"));
    effort(&mut a3, "xhigh");
    jsonl(&[
        user("u1", None, 0, json!("First.")),
        a1,
        user("u2", Some("a1"), 2, json!("Second.")),
        a2,
        user("u3", Some("a2"), 4, json!("Third.")),
        a3,
    ])
}

/// A sealed thinking block with a readable summary (a gloss).
fn sealed_source() -> String {
    jsonl(&[
        user("u1", None, 0, json!("Think first.")),
        assistant(
            "a1",
            "u1",
            1,
            "m1",
            thinking("A summary of my thinking."),
            json!(null),
        ),
        assistant("a3", "a1", 3, "m1", said("Thought."), json!("end_turn")),
    ])
}

/// Codex fixtures: `g2` and `g3` of the Codex importer's tests.
mod cx {
    use super::*;

    pub const REASONING: &str = "c2VhbGVkIHJlYXNvbmluZyA_Pn4g-_8=";
    pub const COMPACTION: &str = "c2VhbGVkIGNvbXBhY3Rpb24g--8=";
    pub const SEALED_MESSAGE: &str = "c2VhbGVkIG1lc3NhZ2Ug_g==";
    pub const PNG: &str = "data:image/png;base64,iVBORw0KGgo=";

    pub fn line(second: u32, kind: &str, payload: Value) -> Value {
        json!({"timestamp": stamp(second), "type": kind, "payload": payload})
    }

    pub fn g2() -> String {
        jsonl(&[
            line(
                0,
                "session_meta",
                json!({"id": "33333333-3333-4333-8333-333333333333",
                "timestamp": stamp(0), "cwd": "/synthetic", "originator": "codex_cli_rs",
                "cli_version": "0.0.0", "instructions": null, "source": "cli",
                "model_provider": "openai"}),
            ),
            line(
                1,
                "response_item",
                json!({"type": "message", "role": "developer",
                "content": [{"type": "input_text", "text": "Sandbox: workspace-write."}]}),
            ),
            line(
                2,
                "event_msg",
                json!({"type": "task_started", "turn_id": "t1",
                "model_context_window": 1000, "collaboration_mode_kind": "default"}),
            ),
            line(
                3,
                "turn_context",
                json!({"cwd": "/synthetic", "approval_policy": "on-request",
                "sandbox_policy": {"type": "workspace-write", "network_access": false},
                "model": "gpt-5-codex", "effort": "high", "summary": "auto",
                "user_instructions": "Sandbox: workspace-write."}),
            ),
            line(
                4,
                "response_item",
                json!({"type": "message", "role": "user",
                "content": [{"type": "input_text", "text": "Fix the bug."}]}),
            ),
            line(
                4,
                "event_msg",
                json!({"type": "user_message", "message": "Fix the bug.",
                "images": [], "local_images": [], "text_elements": []}),
            ),
            line(
                5,
                "response_item",
                json!({"type": "reasoning", "summary": [
                {"type": "summary_text", "text": "**Looking** at it."}],
                "content": null, "encrypted_content": REASONING}),
            ),
            line(
                5,
                "event_msg",
                json!({"type": "agent_reasoning", "text": "**Looking** at it."}),
            ),
            line(
                6,
                "event_msg",
                json!({"type": "agent_message", "message": "Checking the parser.",
                "phase": "commentary"}),
            ),
            line(
                6,
                "response_item",
                json!({"type": "message", "role": "assistant", "phase": "commentary",
                "content": [{"type": "output_text", "text": "Checking the parser."}]}),
            ),
            line(
                7,
                "event_msg",
                json!({"type": "token_count", "info": null, "rate_limits": null}),
            ),
            line(
                8,
                "event_msg",
                json!({"type": "turn_aborted", "reason": "interrupted", "turn_id": "t1"}),
            ),
            line(
                9,
                "event_msg",
                json!({"type": "task_started", "turn_id": "t2"}),
            ),
            line(
                9,
                "turn_context",
                json!({"cwd": "/synthetic", "model": "gpt-5-codex",
                "effort": "xhigh", "summary": "auto"}),
            ),
            line(
                10,
                "response_item",
                json!({"type": "message", "role": "user",
                "content": [{"type": "input_text", "text": "Go on."}]}),
            ),
            line(
                11,
                "event_msg",
                json!({"type": "task_complete", "turn_id": "t2",
                "last_agent_message": null,
                "error": {"message": "stream disconnected", "codex_error_info": "other"}}),
            ),
            line(
                12,
                "compacted",
                json!({"message": "The bug is in the parser."}),
            ),
            line(
                13,
                "response_item",
                json!({"type": "message", "role": "assistant", "phase": "final_answer",
                "content": [{"type": "output_text", "text": "Fixed."}]}),
            ),
        ])
    }

    pub fn g3() -> String {
        jsonl(&[
            line(
                0,
                "session_meta",
                json!({"id": "44444444-4444-4444-8444-444444444444",
                "timestamp": stamp(0), "cwd": "/synthetic", "originator": "codex_exec",
                "cli_version": "0.0.0", "model_provider": "openai",
                "base_instructions": {"text": "You are a coding agent."},
                "agent_nickname": "Ada", "agent_path": "/root/ada",
                "parent_thread_id": "55555555-5555-4555-8555-555555555555",
                "source": {"subagent": {"thread_spawn": {
                    "parent_thread_id": "55555555-5555-4555-8555-555555555555",
                    "depth": 1, "agent_nickname": "Ada", "agent_path": "/root/ada",
                    "agent_role": null}}},
                "history_mode": "legacy", "multi_agent_version": "2",
                "context_window": {"window_id": "w1"}}),
            ),
            line(
                1,
                "world_state",
                json!({"full": true, "state": {
                "agents_md": {"directory": "/synthetic", "text": "Use tabs."},
                "apps_instructions": false, "skills": {"includeInstructions": true}}}),
            ),
            line(
                1,
                "response_item",
                json!({"type": "message", "role": "developer",
                "content": [{"type": "input_text",
                    "text": "Instructions for /synthetic:\nUse tabs."}]}),
            ),
            line(
                2,
                "turn_context",
                json!({"cwd": "/synthetic", "model": "gpt-5.5", "effort": "ultra",
                "summary": "detailed", "collaboration_mode": {"mode": "default",
                    "settings": {"model": "gpt-5.5", "reasoning_effort": "ultra",
                        "developer_instructions": null}}}),
            ),
            line(
                3,
                "inter_agent_communication_metadata",
                json!({"trigger_turn": true}),
            ),
            line(
                3,
                "response_item",
                json!({"type": "agent_message", "author": "/root",
                "recipient": "/root/ada",
                "internal_chat_message_metadata_passthrough": {"turn_id": "t1"},
                "content": [{"type": "input_text", "text": "Count the lines."},
                    {"type": "encrypted_content", "encrypted_content": SEALED_MESSAGE}]}),
            ),
            line(
                4,
                "response_item",
                json!({"type": "function_call", "name": "wc",
                "namespace": "mcp__tools", "arguments": "{\"path\":\"a.txt\"}",
                "call_id": "c1",
                "internal_chat_message_metadata_passthrough": {"turn_id": "t1"}}),
            ),
            line(
                5,
                "response_item",
                json!({"type": "function_call_output", "call_id": "c1",
                "output": [{"type": "input_text", "text": "3 a.txt"},
                    {"type": "input_image", "image_url": PNG, "detail": "original"}]}),
            ),
            line(
                6,
                "response_item",
                json!({"type": "custom_tool_call", "status": "completed",
                "call_id": "c2", "name": "apply_patch",
                "input": "*** Begin Patch\n*** End Patch\n"}),
            ),
            line(
                7,
                "response_item",
                json!({"type": "custom_tool_call_output", "call_id": "c2",
                "output": "Done!"}),
            ),
            line(
                8,
                "response_item",
                json!({"type": "web_search_call", "status": "completed",
                "action": {"type": "search", "query": "line counts"}}),
            ),
            line(
                9,
                "compacted",
                json!({"message": "",
                "replacement_history": [
                    {"type": "message", "role": "developer",
                     "content": [{"type": "input_text",
                        "text": "Instructions for /synthetic:\nUse tabs."}]},
                    {"type": "agent_message", "author": "/root", "recipient": "/root/ada",
                     "content": [{"type": "input_text", "text": "Count the lines."}]},
                    {"type": "compaction", "encrypted_content": COMPACTION}
                ],
                "window_id": "w2", "previous_window_id": "w1", "first_window_id": "w1",
                "window_number": 2}),
            ),
            line(
                10,
                "response_item",
                json!({"type": "message", "role": "assistant",
                "phase": "final_answer",
                "content": [{"type": "output_text", "text": "Three lines."}]}),
            ),
        ])
    }
}

/// What became of one wrong output.
enum Outcome {
    /// The gate's report on it.
    Gated(Report),
    /// The importer refused the source.
    Refused,
}

/// Every round-3 case, by name, with what became of it.
fn round3_cases() -> Vec<(&'static str, Outcome)> {
    use crate::archive_v3::codex::import as cx_import;
    let mut out: Vec<(&'static str, Outcome)> = Vec::new();
    let mut note = |name: &'static str, report: Report| out.push((name, Outcome::Gated(report)));

    // ---- Claude Code
    let s = source();
    let fresh = || import_ok(&s);

    // CC-1 gloss generated_by invented (C2 regression), a model the source names.
    {
        let mut f = fresh().fragment;
        let (g, _) = gloss_on(&f, v3::gloss::category::TITLE);
        f += av3::annotate_gloss(g, "claude-opus-4-6").unwrap();
        note("CC-1 title gloss generated_by invented", run(&s, f));
    }
    // CC-2 occurrence author/experiencer invented.
    {
        let i = fresh();
        let hi = one(&i, "Hi.");
        let mut f = i.fragment;
        let o = occ_of(&f, witness_of(&f, hi)[0])[0];
        add(
            &mut f,
            Trible::force(&o, &v3::witness::author.id(), &gv(FRESH_A)),
        );
        add(
            &mut f,
            Trible::force(&o, &v3::witness::experiencer.id(), &gv(FRESH_B)),
        );
        note("CC-2 occurrence author+experiencer invented", run(&s, f));
    }
    // CC-3 witness source_namespace changed.
    {
        let i = fresh();
        let hi = one(&i, "Hi.");
        let mut f = i.fragment;
        let w = witness_of(&f, hi)[0];
        set_id(
            &mut f,
            w,
            v3::witness::source_namespace.id(),
            SOURCE_CHATGPT,
        );
        note("CC-3 witness source_namespace swapped", run(&s, f));
    }
    // CC-4 a gloss without category carrying invented text.
    {
        let i = fresh();
        let again = one(&i, "Again.");
        let mut f = i.fragment;
        categoryless_gloss(&mut f, again, "Invented summary.", FRESH_C);
        note("CC-4 categoryless gloss with invented text", run(&s, f));
    }
    // CC-5 a node that holds nothing spliced into her window.
    {
        let i = fresh();
        let hi = one(&i, "Hi.");
        let hello = witnessed(&i, "Hello.")
            .into_iter()
            .find(|n| prev(&i.fragment, *n).first() == Some(&hi))
            .unwrap();
        let mut f = i.fragment;
        splice_empty(&mut f, hi, hello, FRESH_D);
        note("CC-5 holds-nothing node spliced into a window", run(&s, f));
    }
    // CC-6 invented media type pointing at a nameless entity.
    {
        let i = fresh();
        let again = one(&i, "Again.");
        let mut f = i.fragment;
        let fact = holds(&f, again)[0];
        add(
            &mut f,
            Trible::force(&fact, &v3::fact::media_type.id(), &gv(FRESH_E)),
        );
        note(
            "CC-6 media_type to a nameless entity on a text fact",
            run(&s, f),
        );
    }
    // CC-7 generated_by on a user occurrence, nameless model entity.
    {
        let i = fresh();
        let again = one(&i, "Again.");
        let mut f = i.fragment;
        let o = occ_of(&f, witness_of(&f, again)[0])[0];
        add(
            &mut f,
            Trible::force(&o, &v3::witness::generated_by.id(), &gv(FRESH_E)),
        );
        note(
            "CC-7 occurrence generated_by to a nameless model",
            run(&s, f),
        );
    }
    // CC-8 an occurrence duplicated under another entity, same claims.
    {
        let i = fresh();
        let again = one(&i, "Again.");
        let mut f = i.fragment;
        let o = occ_of(&f, witness_of(&f, again)[0])[0];
        clone_entity(&mut f, o, FRESH_A);
        note(
            "CC-8 occurrence duplicated (same claims, second entity)",
            run(&s, f),
        );
    }
    // CC-9 the title gloss duplicated on the same node (second entity).
    {
        let mut f = fresh().fragment;
        let (g, _) = gloss_on(&f, v3::gloss::category::TITLE);
        clone_entity(&mut f, g, FRESH_B);
        note("CC-9 title gloss doubled on its own node", run(&s, f));
    }
    // CC-10 ends:end_turn moved between the two records of one message.
    {
        let i = fresh();
        let second = second_done(&i);
        let mut f = i.fragment;
        let first = prev(&f, second)[0];
        let e = of(f.facts(), second, v3::node::ends.id());
        remove(&mut f, &e);
        add(
            &mut f,
            Trible::force(&first, &v3::node::ends.id(), &gv(v3::node::ends::END_TURN)),
        );
        note(
            "CC-10 ends moved to the earlier record of one message",
            run(&s, f),
        );
    }
    // CC-11 a timestamp moved from its occurrence onto the witness.
    {
        let i = fresh();
        let again = one(&i, "Again.");
        let mut f = i.fragment;
        let w = witness_of(&f, again)[0];
        let o = occ_of(&f, w)[0];
        let t = of(f.facts(), o, v3::witness::timestamp.id());
        remove(&mut f, &t);
        for t in &t {
            add(&mut f, Trible::force(&w, t.a(), t.v::<UnknownInline>()));
        }
        note(
            "CC-11 timestamp moved from occurrence to witness",
            run(&s, f),
        );
    }

    // Compaction.
    let sc = compaction_source();
    let freshc = || import_ok(&sc);
    // CC-12 continues moved one node down the new line.
    {
        let i = freshc();
        let third = one(&i, "Third.");
        let mut f = i.fragment;
        let (from, to) = subjects_of(f.facts(), v3::node::continues.id())[0];
        let t = of(f.facts(), from, v3::node::continues.id());
        remove(&mut f, &t);
        add(
            &mut f,
            Trible::force(&third, &v3::node::continues.id(), &gv(to)),
        );
        note(
            "CC-12 continues moved from the summary to the next node",
            run(&sc, f),
        );
    }
    // CC-13 continues retargeted to the old tail's predecessor.
    {
        let i = freshc();
        let second = one(&i, "Second.");
        let mut f = i.fragment;
        let (from, _) = subjects_of(f.facts(), v3::node::continues.id())[0];
        set_id(&mut f, from, v3::node::continues.id(), second);
        note(
            "CC-13 continues names the old tail's predecessor",
            run(&sc, f),
        );
    }
    // CC-14 the preserved copy recalls the summary instead of its original.
    {
        let i = freshc();
        let summary = one(&i, "Summary of what came before.");
        let mut f = i.fragment;
        let (from, _) = subjects_of(f.facts(), v3::node::recalls.id())[0];
        set_id(&mut f, from, v3::node::recalls.id(), summary);
        note("CC-14 recalls retargeted to the summary", run(&sc, f));
    }
    // CC-15 continues added a second time on the copy (two continues).
    {
        let mut f = freshc().fragment;
        let (_, to) = subjects_of(f.facts(), v3::node::continues.id())[0];
        let (copy, _) = subjects_of(f.facts(), v3::node::recalls.id())[0];
        add(
            &mut f,
            Trible::force(&copy, &v3::node::continues.id(), &gv(to)),
        );
        note(
            "CC-15 a second continues on the preserved copy",
            run(&sc, f),
        );
    }

    // S2.
    let sr = rewrite_source();
    let freshr = || import_ok(&sr);
    let her_call = |f: &Fragment| {
        let sent = subjects_with(f.facts(), v3::fact::form.id(), v3::fact::form::REWRITTEN);
        let calls = subjects_with(
            f.facts(),
            v3::fact::modality.id(),
            v3::fact::modality::TOOL_CALL,
        );
        let hers = calls.into_iter().find(|c| *c != sent[0]).unwrap();
        (sent[0], hers)
    };
    // CC-16 the sent form recalls her call.
    {
        let mut f = freshr().fragment;
        let (sent, hers) = her_call(&f);
        let sent_node = subjects_with(f.facts(), v3::node::holds.id(), sent)[0];
        let her_node = subjects_with(f.facts(), v3::node::holds.id(), hers)[0];
        add(
            &mut f,
            Trible::force(&sent_node, &v3::node::recalls.id(), &gv(her_node)),
        );
        note("CC-16 the wire form recalls her call", run(&sr, f));
    }
    // CC-17 the result copy after the sent form answers her own call.
    {
        let i = freshr();
        let copy = unwitnessed(&i, "listing");
        let mut f = i.fragment;
        let (_, hers) = her_call(&f);
        let her_node = subjects_with(f.facts(), v3::node::holds.id(), hers)[0];
        add(
            &mut f,
            Trible::force(&copy[0], &v3::node::responds_to.id(), &gv(her_node)),
        );
        note(
            "CC-17 the copied result gains responds_to to her call outside its window",
            run(&sr, f),
        );
    }
    // CC-18 the wire form stored as her call (her call's structure replaced).
    {
        let mut f = freshr().fragment;
        let (sent, hers) = her_call(&f);
        let s_struct = ids(f.facts(), sent, v3::fact::structure.id())[0];
        set_id(&mut f, hers, v3::fact::structure.id(), s_struct);
        note("CC-18 her call holds the wire form", run(&sr, f));
    }

    // Effort.
    let se = effort_source();
    let freshe = || import_ok(&se);
    // CC-19 her xhigh answer grafted under the high line.
    {
        let i = freshe();
        let three = one(&i, "Three.");
        let two = one(&i, "Two.");
        let mut f = i.fragment;
        let p = prev(&f, three)[0];
        set_id(&mut f, p, v3::node::previous.id(), two);
        note(
            "CC-19 xhigh turn grafted under the high header line",
            run(&se, f),
        );
    }
    // CC-20 the two effort headers swapped.
    {
        let mut f = freshe().fragment;
        let headers = subjects_of(f.facts(), v3::fact::effort.id());
        let (h1, e1) = headers[0];
        let (h2, e2) = headers[1];
        set_id(&mut f, h1, v3::fact::effort.id(), e2);
        set_id(&mut f, h2, v3::fact::effort.id(), e1);
        note(
            "CC-20 effort terms swapped between the two headers",
            run(&se, f),
        );
    }

    // Sealed thinking.
    let ss = sealed_source();
    let freshs = || import_ok(&ss);
    // CC-21 the reasoning gloss moved from the sealed node to her answer.
    {
        let i = freshs();
        let thought = one(&i, "Thought.");
        let mut f = i.fragment;
        let (g, _) = gloss_on(&f, v3::gloss::category::REASONING_SUMMARY);
        set_id(&mut f, g, v3::gloss::describes.id(), thought);
        note("CC-21 reasoning gloss moved to the answer", run(&ss, f));
    }
    // CC-22 the sealed blob stored as its base64 text.
    {
        let mut f = freshs().fragment;
        let fact = subjects_any(f.facts(), v3::fact::blob.id())[0];
        let old = of(f.facts(), fact, v3::fact::blob.id());
        remove(&mut f, &old);
        let h: Inline<Handle<RawBytes>> = f.put::<RawBytes, _>(anybytes::Bytes::from_source(
            b"c2lnbmVkIHRoaW5raW5n".to_vec(),
        ));
        add(&mut f, Trible::force(&fact, &v3::fact::blob.id(), &h));
        note("CC-22 sealed blob kept undecoded", run(&ss, f));
    }
    // CC-23 report: a key spelled like a placeholder.
    {
        let mut a1: Value = serde_json::from_str(s.lines().nth(1).unwrap()).unwrap();
        a1["<hunter2pass>"] = json!(1);
        let mut lines: Vec<String> = s.lines().map(str::to_owned).collect();
        lines[1] = serde_json::to_string(&a1).unwrap();
        let mutated = lines.join("\n") + "\n";
        let failure = import(mutated.as_bytes())
            .err()
            .expect("the importer refuses");
        let printed = gate::vocabulary(&TABLE, &failure.path);
        assert!(leaks(&printed).is_empty(), "CC-23: {printed}");
        let typed = s.replacen("\"type\":\"user\"", "\"type\":\"<hunter2pass>\"", 1);
        let failure = import(typed.as_bytes())
            .err()
            .expect("the importer refuses");
        let printed = gate::vocabulary(&TABLE, &failure.path);
        assert!(leaks(&printed).is_empty(), "CC-23b: {printed}");
        let typed_report = run(&typed, fresh().fragment);
        assert!(!typed_report.passed());
        assert!(leaks(&format!("{typed_report}")).is_empty());
        note(
            "CC-23 placeholder-shaped key (gate report)",
            run(&mutated, fresh().fragment),
        );
    }

    // ---- Codex
    let x = cx::g2();
    let freshx = || import_with(cx_import, &x);
    let runx = |f: Fragment| run_on(&CX, &x, f);

    // CX-1 reasoning gloss generated_by, a model the source names.
    {
        let mut f = freshx().fragment;
        let (g, _) = gloss_on(&f, v3::gloss::category::REASONING_SUMMARY);
        f += av3::annotate_gloss(g, "gpt-5-codex").unwrap();
        note("CX-1 reasoning gloss generated_by invented", runx(f));
    }
    // CX-2 ends:interrupted on his prompt instead of her commentary (X5).
    {
        let i = freshx();
        let fix = one(&i, "Fix the bug.");
        let mut f = i.fragment;
        let (from, _) = subjects_of(f.facts(), v3::node::ends.id())[0];
        let e = of(f.facts(), from, v3::node::ends.id());
        remove(&mut f, &e);
        add(
            &mut f,
            Trible::force(&fix, &v3::node::ends.id(), &gv(v3::node::ends::INTERRUPTED)),
        );
        note("CX-2 ends:interrupted on his prompt", runx(f));
    }
    // CX-3 ends:error on the erred turn's prompt (X6).
    {
        let i = freshx();
        let go = one(&i, "Go on.");
        let mut f = i.fragment;
        add(
            &mut f,
            Trible::force(&go, &v3::node::ends.id(), &gv(v3::node::ends::ERROR)),
        );
        note("CX-3 ends:error on his prompt", runx(f));
    }
    // CX-4 continues moved to the next node.
    {
        let i = freshx();
        let fixed = one(&i, "Fixed.");
        let mut f = i.fragment;
        let (from, to) = subjects_of(f.facts(), v3::node::continues.id())[0];
        let t = of(f.facts(), from, v3::node::continues.id());
        remove(&mut f, &t);
        add(
            &mut f,
            Trible::force(&fixed, &v3::node::continues.id(), &gv(to)),
        );
        note("CX-4 continues moved down the new line", runx(f));
    }
    // CX-5 continues names an ancestor of the old tail.
    {
        let i = freshx();
        let checking = one(&i, "Checking the parser.");
        let mut f = i.fragment;
        let (from, _) = subjects_of(f.facts(), v3::node::continues.id())[0];
        set_id(&mut f, from, v3::node::continues.id(), checking);
        note("CX-5 continues names an ancestor of the old tail", runx(f));
    }
    // CX-6 sealed reasoning stored undecoded.
    {
        let mut f = freshx().fragment;
        let fact = subjects_any(f.facts(), v3::fact::blob.id())[0];
        let old = of(f.facts(), fact, v3::fact::blob.id());
        remove(&mut f, &old);
        let h: Inline<Handle<RawBytes>> = f.put::<RawBytes, _>(anybytes::Bytes::from_source(
            cx::REASONING.as_bytes().to_vec(),
        ));
        add(&mut f, Trible::force(&fact, &v3::fact::blob.id(), &h));
        note("CX-6 sealed reasoning kept as base64 text", runx(f));
    }
    // CX-7 sealed media type dropped.
    {
        let mut f = freshx().fragment;
        let fact = subjects_any(f.facts(), v3::fact::blob.id())[0];
        let m = of(f.facts(), fact, v3::fact::media_type.id());
        remove(&mut f, &m);
        note("CX-7 sealed blob loses its media type", runx(f));
    }
    // CX-8 media type to a nameless entity on her answer.
    {
        let i = freshx();
        let fixed = one(&i, "Fixed.");
        let mut f = i.fragment;
        let fact = holds(&f, fixed)[0];
        add(
            &mut f,
            Trible::force(&fact, &v3::fact::media_type.id(), &gv(FRESH_E)),
        );
        note("CX-8 media_type to a nameless entity", runx(f));
    }
    // CX-9 occurrence author invented.
    {
        let i = freshx();
        let fix = one(&i, "Fix the bug.");
        let mut f = i.fragment;
        let o = occ_of(&f, witness_of(&f, fix)[0])[0];
        add(
            &mut f,
            Trible::force(&o, &v3::witness::author.id(), &gv(FRESH_A)),
        );
        note("CX-9 occurrence author invented", runx(f));
    }
    // CX-10 witness namespace swapped.
    {
        let i = freshx();
        let fix = one(&i, "Fix the bug.");
        let mut f = i.fragment;
        let w = witness_of(&f, fix)[0];
        set_id(
            &mut f,
            w,
            v3::witness::source_namespace.id(),
            SOURCE_CHATGPT,
        );
        note("CX-10 witness source_namespace swapped", runx(f));
    }
    // CX-11 categoryless gloss with invented text.
    {
        let i = freshx();
        let fixed = one(&i, "Fixed.");
        let mut f = i.fragment;
        categoryless_gloss(&mut f, fixed, "Invented summary.", FRESH_C);
        note("CX-11 categoryless gloss with invented text", runx(f));
    }
    // CX-12 "Go on." grafted under the high line (effort branch point).
    {
        let i = freshx();
        let go = one(&i, "Go on.");
        let checking = one(&i, "Checking the parser.");
        let mut f = i.fragment;
        set_id(&mut f, go, v3::node::previous.id(), checking);
        note("CX-12 xhigh prompt grafted under the high line", runx(f));
    }
    // CX-13 holds-nothing node spliced in.
    {
        let i = freshx();
        let fix = one(&i, "Fix the bug.");
        let mut f = i.fragment;
        let after = next(&f, fix);
        splice_empty(&mut f, fix, after[0], FRESH_D);
        note("CX-13 holds-nothing node spliced into a window", runx(f));
    }
    // CX-14 the re-presented prompt loses its recalls.
    {
        let mut f = freshx().fragment;
        let r: Vec<Trible> = f
            .facts()
            .iter()
            .filter(|t| *t.a() == v3::node::recalls.id())
            .copied()
            .collect();
        remove(&mut f, &r[..1]);
        note("CX-14 a re-presented copy loses its recalls", runx(f));
    }
    // CX-15 duplicate occurrence, same claims.
    {
        let i = freshx();
        let fix = one(&i, "Fix the bug.");
        let mut f = i.fragment;
        let o = occ_of(&f, witness_of(&f, fix)[0])[0];
        clone_entity(&mut f, o, FRESH_B);
        note("CX-15 occurrence duplicated (same claims)", runx(f));
    }
    // CX-16 the turn_ends claim of the abort moved to another occurrence.
    {
        let i = freshx();
        let fix = one(&i, "Fix the bug.");
        let mut f = i.fragment;
        let te: Vec<Trible> = f
            .facts()
            .iter()
            .filter(|t| *t.a() == v3::witness::turn_ends.id())
            .copied()
            .collect();
        let t = te[0];
        remove(&mut f, &[t]);
        let o = occ_of(&f, witness_of(&f, fix)[0])[0];
        add(&mut f, Trible::force(&o, t.a(), t.v::<UnknownInline>()));
        note(
            "CX-16 abort claim moved to his prompt's occurrence",
            runx(f),
        );
    }
    // CX-17 register dropped where only the event carries the phase (X1).
    {
        let records: Vec<Value> = x
            .lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .map(|mut v| {
                if v["type"] == "response_item" && v["payload"]["phase"] == "commentary" {
                    v["payload"].as_object_mut().unwrap().remove("phase");
                }
                v
            })
            .collect();
        let only_event = jsonl(&records);
        assert_ne!(only_event, x);
        let baseline = run_on(
            &CX,
            &only_event,
            import_with(cx_import, &only_event).fragment,
        );
        assert!(baseline.passed(), "CX-17 baseline: {baseline}");
        let i = import_with(cx_import, &only_event);
        let checking = one(&i, "Checking the parser.");
        let mut f = i.fragment;
        let fact = holds(&f, checking)[0];
        let r = of(f.facts(), fact, v3::fact::register.id());
        assert_eq!(r.len(), 1, "the event's phase sets the register");
        remove(&mut f, &r);
        note(
            "CX-17 register lost where only the event names the phase",
            run_on(&CX, &only_event, f),
        );
    }

    // g3: pointer, spawned roots.
    let x3 = cx::g3();
    let fresh3 = || import_with(cx_import, &x3);
    let run3 = |f: Fragment| run_on(&CX, &x3, f);
    // CX-18 the parent-thread pointer claim moved to her answer's occurrence.
    {
        let i = fresh3();
        let three = one(&i, "Three lines.");
        let mut f = i.fragment;
        let p: Vec<Trible> = f
            .facts()
            .iter()
            .filter(|t| *t.a() == v3::witness::pointer.id())
            .copied()
            .collect();
        assert!(!p.is_empty());
        let o = occ_of(&f, witness_of(&f, three)[0])[0];
        for t in &p {
            remove(&mut f, &[*t]);
            add(&mut f, Trible::force(&o, t.a(), t.v::<UnknownInline>()));
        }
        note("CX-18 parent pointer moved to another occurrence", run3(f));
    }
    // CX-19 a spawned root (guardian shape) holding an existing answer,
    // witnessed with that answer's claims, spawned_by the wc call.
    {
        let i = fresh3();
        let three = one(&i, "Three lines.");
        let mut f = i.fragment;
        let fact = holds(&f, three)[0];
        let call_fact = subjects_with(
            f.facts(),
            v3::fact::modality.id(),
            v3::fact::modality::TOOL_CALL,
        )[0];
        let call_node = subjects_with(f.facts(), v3::node::holds.id(), call_fact)[0];
        let root = FRESH_A;
        add(
            &mut f,
            Trible::force(&root, &v3::node::holds.id(), &gv(fact)),
        );
        add(
            &mut f,
            Trible::force(&root, &metadata::tag.id(), &gv(v3::node::KIND)),
        );
        add(
            &mut f,
            Trible::force(&root, &v3::node::spawned_by.id(), &gv(call_node)),
        );
        let w = av3::witness(SOURCE_CODEX, root).unwrap();
        let w_id = w.root().unwrap();
        f += w;
        let o = occ_of(&f, witness_of(&f, three)[0])[0];
        for t in entity(f.facts(), o) {
            let v = if *t.a() == v3::occurrence::of.id() {
                gv(w_id).transmute()
            } else {
                *t.v::<UnknownInline>()
            };
            add(&mut f, Trible::force(&FRESH_B, t.a(), &v));
        }
        note("CX-19 witnessed spawned root re-using her answer", run3(f));
    }
    // CX-20 spawned_by added on the line's root (to the call).
    {
        let i = fresh3();
        let three = one(&i, "Three lines.");
        let mut f = i.fragment;
        let call_fact = subjects_with(
            f.facts(),
            v3::fact::modality.id(),
            v3::fact::modality::TOOL_CALL,
        )[0];
        let call_node = subjects_with(f.facts(), v3::node::holds.id(), call_fact)[0];
        let mut root = three;
        while let Some(p) = prev(&f, root).first().copied() {
            root = p;
        }
        add(
            &mut f,
            Trible::force(&root, &v3::node::spawned_by.id(), &gv(call_node)),
        );
        note("CX-20 spawned_by on the compacted line's root", run3(f));
    }
    // CX-21 report: a placeholder-shaped key.
    {
        let mut v: Vec<Value> = x
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        v[3]["payload"]["<hunter2pass>"] = json!(1);
        let mutated = jsonl(&v);
        let failure = cx_import(mutated.as_bytes())
            .err()
            .expect("the importer refuses");
        let printed = gate::vocabulary(&CX, &failure.path);
        assert!(leaks(&printed).is_empty(), "CX-21: {printed}");
        note(
            "CX-21 placeholder-shaped key (gate report)",
            run_on(&CX, &mutated, freshx().fragment),
        );
    }
    // CX-22 an input_text part without text; CX-22b user content that is a
    // string, not a list.
    let parts: [(&'static str, fn(&mut Value)); 2] = [
        ("CX-22 input_text without text", |v| {
            v["payload"]["content"]
                .as_array_mut()
                .unwrap()
                .push(json!({"type": "input_text"}))
        }),
        ("CX-22b message content a string", |v| {
            v["payload"]["content"] = json!("Fix the bug.")
        }),
    ];
    for (name, mutate) in parts {
        let mut v: Vec<Value> = x
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        mutate(&mut v[4]);
        let mutated = jsonl(&v);
        match cx_import(mutated.as_bytes()) {
            Err(failure) => {
                let printed = gate::vocabulary(&CX, &failure.path);
                assert!(leaks(&printed).is_empty(), "{name}: {printed}");
                out.push((name, Outcome::Refused));
            }
            Ok(fragment) => out.push((name, Outcome::Gated(run_on(&CX, &mutated, fragment)))),
        }
    }
    out
}

#[test]
fn the_gate_catches_every_round3_output() {
    use crate::archive_v3::codex::import as cx_import;
    for (table, source, importer) in [
        (&TABLE, source(), import as Importer),
        (&TABLE, compaction_source(), import),
        (&TABLE, rewrite_source(), import),
        (&TABLE, effort_source(), import),
        (&TABLE, sealed_source(), import),
        (&CX, cx::g2(), cx_import),
        (&CX, cx::g3(), cx_import),
    ] {
        let report = run_on(table, &source, import_with(importer, &source).fragment);
        assert!(report.passed(), "baseline: {report}");
    }
    let cases = round3_cases();
    assert_eq!(cases.len(), 46);
    let mut missed = Vec::new();
    for (name, outcome) in &cases {
        match outcome {
            Outcome::Refused => println!("caught\t{name}\t[importer]"),
            Outcome::Gated(report) if report.passed() => {
                println!("MISSED\t{name}");
                missed.push(*name);
            }
            Outcome::Gated(report) => {
                let mut kinds: Vec<String> =
                    report.rows.iter().map(|r| r.kind.to_string()).collect();
                kinds.sort();
                kinds.dedup();
                println!("caught\t{name}\t[{}]", kinds.join(","));
                let text = format!("{report}");
                assert!(leaks(&text).is_empty(), "{name} printed a value:\n{text}");
            }
        }
    }
    assert!(
        missed.is_empty(),
        "the gate missed {} of 46: {missed:?}",
        missed.len()
    );
}

// ---------------------------------------------------------------------------
// Round 4: `ends` per occurrence (M1), a node that holds nothing (G11), and
// counts (G12).

fn import_raw(import: Importer, source: &str) -> Imported {
    let mut fragment = import(source.as_bytes()).unwrap_or_else(|failure| panic!("{failure}"));
    let blobs = fragment.blobs_mut().snapshot().unwrap();
    Imported { fragment, blobs }
}

/// The output as importers placed it before M1: every occurrence's `ends`
/// moved onto the node its witness projects to. The importer may already
/// place them either way; the attack must move at least one.
fn ends_on_nodes(mut f: Fragment) -> Fragment {
    let occurrences: Vec<Id> = f
        .facts()
        .iter()
        .filter(|t| *t.a() == v3::occurrence::of.id())
        .map(|t| *t.e())
        .collect();
    let mut moved = 0;
    for occurrence in occurrences {
        let ends = of(f.facts(), occurrence, v3::witness::ends.id());
        for witness in ids(f.facts(), occurrence, v3::occurrence::of.id()) {
            for node in ids(f.facts(), witness, v3::witness::projects_to.id()) {
                for t in &ends {
                    add(&mut f, Trible::force(&node, t.a(), t.v::<GenId>()));
                    moved += 1;
                }
            }
        }
        remove(&mut f, &ends);
    }
    let on_nodes = f
        .facts()
        .iter()
        .filter(|t| *t.a() == v3::node::ends.id())
        .count();
    assert!(moved > 0 || on_nodes > 0, "no ends to move onto a node");
    f
}

/// A regenerated answer: two records with the same text after one prompt,
/// one stopping at its end and one at its token limit. They converge on one
/// node.
fn regenerated_source() -> String {
    jsonl(&[
        user("u1", None, 0, json!("Hi.")),
        assistant("a1", "u1", 1, "m1", said("Hello."), json!("end_turn")),
        assistant("a2", "u1", 2, "m2", said("Hello."), json!("max_tokens")),
    ])
}

/// The occurrences of a node, earliest record first.
fn occurrences_by_time(f: &Fragment, node: Id) -> Vec<Id> {
    use triblespace::prelude::inlineencodings::NsTAIInterval;
    let mut found: Vec<(i128, Id)> = witness_of(f, node)
        .into_iter()
        .flat_map(|w| occ_of(f, w))
        .map(|o| {
            let time = of(f.facts(), o, v3::witness::timestamp.id())[0]
                .v::<NsTAIInterval>()
                .try_from_inline::<(i128, i128)>()
                .unwrap()
                .0;
            (time, o)
        })
        .collect();
    found.sort();
    found.into_iter().map(|(_, o)| o).collect()
}

/// The right output of [`regenerated_source`]: each record's stop on its
/// own occurrence, none on the node.
fn regenerated() -> (Imported, Id, [Id; 2]) {
    let s = regenerated_source();
    let i = import_raw(import, &s);
    let hello = one(&i, "Hello.");
    let mut f = i.fragment;
    let on_node = of(f.facts(), hello, v3::node::ends.id());
    remove(&mut f, &on_node);
    let occurrences = occurrences_by_time(&f, hello);
    assert_eq!(occurrences.len(), 2, "two records hold the answer");
    for (occurrence, term) in occurrences
        .iter()
        .zip([v3::node::ends::END_TURN, v3::node::ends::MAX_TOKENS])
    {
        add(
            &mut f,
            Trible::force(occurrence, &v3::witness::ends.id(), &gv(term)),
        );
    }
    let blobs = f.blobs_mut().snapshot().unwrap();
    (
        Imported { fragment: f, blobs },
        hello,
        [occurrences[0], occurrences[1]],
    )
}

fn round4_cases() -> Vec<(&'static str, Report)> {
    use crate::archive_v3::codex::import as cx_import;
    let mut out: Vec<(&'static str, Report)> = Vec::new();
    let s = source();
    let fresh = || import_ok(&s);

    // M1a the stop moved between the two records of one message.
    {
        let i = fresh();
        let second = second_done(&i);
        let mut f = i.fragment;
        let first = prev(&f, second)[0];
        let from = occ_of(&f, witness_of(&f, second)[0])[0];
        let to = occ_of(&f, witness_of(&f, first)[0])[0];
        let e = ends_of(&f, from);
        assert_eq!(e.len(), 1);
        remove(&mut f, &e);
        add(&mut f, Trible::force(&to, e[0].a(), e[0].v::<GenId>()));
        out.push((
            "M1a ends moved to the other record's occurrence",
            run(&s, f),
        ));
    }
    // M1b the stop on the node, as the importers placed it before M1.
    out.push((
        "M1b ends on the node, not on its occurrence",
        run(&s, ends_on_nodes(import_raw(import, &s).fragment)),
    ));
    // M1c/M1d a regenerated answer: the two stops swapped between its
    // records, or both on the node.
    let sg = regenerated_source();
    {
        let (i, _, [early, late]) = regenerated();
        let mut f = i.fragment;
        let a = ends_of(&f, early);
        let b = ends_of(&f, late);
        remove(&mut f, &a);
        remove(&mut f, &b);
        add(&mut f, Trible::force(&early, b[0].a(), b[0].v::<GenId>()));
        add(&mut f, Trible::force(&late, a[0].a(), a[0].v::<GenId>()));
        out.push(("M1c a regeneration's two stops swapped", run(&sg, f)));
    }
    out.push((
        "M1d a regeneration's two stops on its one node",
        run(&sg, ends_on_nodes(import_raw(import, &sg).fragment)),
    ));
    // M1e Codex: the abort's stop on his prompt's occurrence (X5 under M1).
    {
        let x = cx::g2();
        let i = import_with(cx_import, &x);
        let fix = one(&i, "Fix the bug.");
        let checking = one(&i, "Checking the parser.");
        let mut f = i.fragment;
        // Her commentary's node also carries the abort's own `turn_ends`
        // occurrence (X15), which has no `ends`: take her record's.
        let from = occ_of(&f, witness_of(&f, checking)[0])
            .into_iter()
            .find(|&occurrence| !ends_of(&f, occurrence).is_empty())
            .expect("her commentary's occurrence ends");
        let to = occ_of(&f, witness_of(&f, fix)[0])[0];
        let e = ends_of(&f, from);
        assert!(!e.is_empty(), "her commentary's occurrence ends");
        remove(&mut f, &e);
        for t in &e {
            add(&mut f, Trible::force(&to, t.a(), t.v::<GenId>()));
        }
        out.push((
            "M1e Codex abort stop on his prompt's occurrence",
            run_on(&CX, &x, f),
        ));
    }
    // G11a a node that holds nothing after the leaf, witnessed.
    {
        let i = fresh();
        let leaf = second_done(&i);
        let mut f = i.fragment;
        add(
            &mut f,
            Trible::force(&FRESH_D, &v3::node::previous.id(), &gv(leaf)),
        );
        add(
            &mut f,
            Trible::force(&FRESH_D, &metadata::tag.id(), &gv(v3::node::KIND)),
        );
        let w = av3::witness(
            ids(
                f.facts(),
                witness_of(&f, leaf)[0],
                v3::witness::source_namespace.id(),
            )[0],
            FRESH_D,
        )
        .unwrap();
        f += w;
        out.push(("G11a a witnessed node that holds nothing", run(&s, f)));
    }
    // G11b a node that holds a value that is no fact.
    {
        let i = fresh();
        let again = one(&i, "Again.");
        let mut f = i.fragment;
        let garbage: Inline<UnknownInline> = Inline::new([0xFF; 32]);
        add(
            &mut f,
            Trible::force(&again, &v3::node::holds.id(), &garbage),
        );
        out.push(("G11b a node holds a value that is no fact", run(&s, f)));
    }
    // G12a the witness duplicated under a second entity.
    {
        let i = fresh();
        let again = one(&i, "Again.");
        let mut f = i.fragment;
        let w = witness_of(&f, again)[0];
        clone_entity(&mut f, w, FRESH_C);
        out.push(("G12a witness duplicated under a second entity", run(&s, f)));
    }
    // G12b one of a regeneration's two occurrences duplicated.
    {
        let (i, _, [_, late]) = regenerated();
        let mut f = i.fragment;
        clone_entity(&mut f, late, FRESH_E);
        out.push(("G12b one of two occurrences duplicated", run(&sg, f)));
    }
    out
}

#[test]
fn the_gate_catches_every_round4_output() {
    let (i, hello, _) = regenerated();
    assert_eq!(witness_of(&i.fragment, hello).len(), 1);
    let report = run(&regenerated_source(), i.fragment);
    assert!(report.passed(), "regenerated baseline: {report}");
    let cases = round4_cases();
    assert_eq!(cases.len(), 9);
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
    assert!(missed.is_empty(), "the gate missed {missed:?}");
}

// ---------------------------------------------------------------------------
// Round 4: the adversary's 50 attacks and 3 controls (G17). 26 wrong
// outputs passed the round-4 gate, all renderer-side: JSON structure nodes,
// named entities, witnesses and occurrences, kind tags, undecodable
// structure links, facts per node and a merged gloss. The controls are a
// break the gate always caught.

mod round4 {
    use super::*;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use triblespace::core::import::json_tree as jt;
    use triblespace::prelude::inlineencodings::U256BE;

    // Fresh ids, each minted with `trible genid` for the round-4 attack.
    const R4_A: Id = triblespace::macros::id_hex!("7F68E249A9E04DB2C0C68453143B346D");
    const R4_B: Id = triblespace::macros::id_hex!("EF667A1167D6A8B2B70B3D43C884EB1B");
    const R4_C: Id = triblespace::macros::id_hex!("D001DD25F3D6CDEA274D9AD50F85294A");
    const R4_D: Id = triblespace::macros::id_hex!("575773881E0A7B8D2DE2D55338E15F12");
    const R4_E: Id = triblespace::macros::id_hex!("59B7A8E6100A5E5B84A816ACEC8819BA");
    const R4_F: Id = triblespace::macros::id_hex!("378EB24706D672798A92124A0136F9C5");

    enum Out {
        Gated(Report),
        Refused(String),
        Crashed(String),
        Baseline(String),
    }

    fn garbage() -> Inline<UnknownInline> {
        Inline::new([0xFF; 32])
    }

    fn put_text(f: &mut Fragment, text: &str) -> Inline<Handle<UTF8String>> {
        f.put::<UTF8String, _>(text.to_owned())
    }

    /// The call fact (tool_name and structure) and its structure root.
    fn call_fact(f: &Fragment) -> (Id, Id) {
        let named: Vec<Id> = subjects_any(f.facts(), v3::fact::tool_name.id());
        subjects_of(f.facts(), v3::fact::structure.id())
            .into_iter()
            .find(|(fact, _)| named.contains(fact))
            .expect("a call fact with a structure")
    }

    fn holder(f: &Fragment, fact: Id) -> Id {
        subjects_with(f.facts(), v3::node::holds.id(), fact)[0]
    }

    fn drop_tag(f: &mut Fragment, e: Id, kind: Id) {
        let gone: Vec<Trible> = of(f.facts(), e, metadata::tag.id())
            .into_iter()
            .filter(|t| t.v::<GenId>().try_from_inline::<Id>().ok() == Some(kind))
            .collect();
        assert!(!gone.is_empty(), "no such tag");
        remove(f, &gone);
    }

    fn field_entry(i: &Imported, root: Id, name: &str) -> Id {
        let f = &i.fragment;
        for entry in ids(f.facts(), root, jt::object_field.id()) {
            for t in of(f.facts(), entry, jt::field_name.id()) {
                let h: Inline<Handle<UTF8String>> = *t.v::<Handle<UTF8String>>();
                if text_of(i, h) == name {
                    return entry;
                }
            }
        }
        panic!("no field {name}");
    }

    fn gated(source: &str, table: &FieldTable, f: Fragment) -> Out {
        match catch_unwind(AssertUnwindSafe(|| run_on(table, source, f))) {
            Ok(report) => Out::Gated(report),
            Err(panic) => Out::Crashed(
                panic
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_default(),
            ),
        }
    }

    /// Import a source the importer should refuse (or whose output the gate
    /// should fail).
    fn fed(import: Importer, table: &FieldTable, source: &str) -> Out {
        match import(source.as_bytes()) {
            Err(failure) => {
                let printed = gate::vocabulary(table, &failure.path);
                if !leaks(&printed).is_empty() {
                    return Out::Crashed(format!("LEAK in failure path: {printed}"));
                }
                Out::Refused(printed)
            }
            Ok(mut fragment) => {
                let blobs = fragment.blobs_mut().snapshot().unwrap();
                Out::Gated(gate::check(
                    table,
                    source.as_bytes(),
                    fragment.facts(),
                    &blobs,
                ))
            }
        }
    }

    fn ok_or_baseline(import: Importer, table: &FieldTable, source: &str) -> Result<Imported, Out> {
        match import(source.as_bytes()) {
            Err(failure) => Err(Out::Baseline(format!("refused at {}", failure.path))),
            Ok(mut fragment) => {
                let blobs = fragment.blobs_mut().snapshot().unwrap();
                let report = gate::check(table, source.as_bytes(), fragment.facts(), &blobs);
                if !report.passed() {
                    return Err(Out::Baseline(format!("baseline fails: {report}")));
                }
                Ok(Imported { fragment, blobs })
            }
        }
    }

    fn call_source(input: Value) -> String {
        jsonl(&[
            user("u1", None, 0, json!("Run.")),
            assistant(
                "a1",
                "u1",
                1,
                "m1",
                call("t1", "Bash", input),
                json!("tool_use"),
            ),
            user("u2", Some("a1"), 2, result("t1", json!("out"))),
            assistant("a2", "u2", 3, "m2", said("Done."), json!("end_turn")),
        ])
    }

    /// Codex: two summary parts on one reasoning item.
    fn cx_two_summaries() -> String {
        let mut v: Vec<Value> = cx::g2()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        v[6]["payload"]["summary"] = json!([
            {"type": "summary_text", "text": "**Looking** at it."},
            {"type": "summary_text", "text": "**Then** fixing."}
        ]);
        let second = cx::line(
            5,
            "event_msg",
            json!({"type": "agent_reasoning", "text": "**Then** fixing."}),
        );
        v.insert(8, second);
        jsonl(&v)
    }

    fn cx_meta(second: u32, id: &str, base: &str) -> Value {
        cx::line(
            second,
            "session_meta",
            json!({"id": id, "timestamp": stamp(second), "cwd": "/synthetic",
                "originator": "codex_cli_rs", "cli_version": "0.0.0", "model_provider": "openai",
                "base_instructions": {"text": base}}),
        )
    }

    fn cx_turn(second: u32, turn: &str) -> [Value; 2] {
        [
            cx::line(
                second,
                "event_msg",
                json!({"type": "task_started", "turn_id": turn}),
            ),
            cx::line(
                second,
                "turn_context",
                json!({"model": "gpt-5.5", "effort": "high", "summary": "auto", "turn_id": turn}),
            ),
        ]
    }

    fn cx_user(second: u32, text: &str) -> Value {
        cx::line(
            second,
            "response_item",
            json!({"type": "message", "role": "user",
                "content": [{"type": "input_text", "text": text}]}),
        )
    }

    fn cx_said(second: u32, text: &str, phase: Option<&str>) -> Value {
        let mut item = json!({"type": "message", "role": "assistant",
            "content": [{"type": "output_text", "text": text}]});
        if let Some(phase) = phase {
            item["phase"] = json!(phase);
        }
        cx::line(second, "response_item", item)
    }

    fn cx_event_said(second: u32, text: &str, phase: &str) -> Value {
        cx::line(
            second,
            "event_msg",
            json!({"type": "agent_message", "message": text, "phase": phase}),
        )
    }

    fn cx_head() -> Vec<Value> {
        let mut r = vec![cx_meta(0, "77777777-7777-4777-8777-777777777777", "Sys.")];
        r.extend(cx_turn(1, "t1"));
        r
    }

    /// X9': a unit with a compaction whose guardian_history is `guardian`.
    fn cx_guardian(guardian: Value) -> String {
        let ask = json!({"type": "message", "role": "user",
            "content": [{"type": "input_text", "text": "A."}]});
        let said = json!({"type": "message", "role": "assistant",
            "content": [{"type": "output_text", "text": "B."}]});
        let mut r = cx_head();
        r.push(cx::line(2, "response_item", ask.clone()));
        r.push(cx::line(
            2,
            "event_msg",
            json!({"type": "user_message", "message": "A.",
            "images": [], "local_images": [], "text_elements": []}),
        ));
        r.push(cx::line(3, "response_item", said));
        r.push(cx::line(
            6,
            "compacted",
            json!({"message": "", "replacement_history": [
                ask, {"type": "compaction", "encrypted_content": cx::COMPACTION}],
                "guardian_history": guardian}),
        ));
        r.push(cx_said(7, "E.", None));
        jsonl(&r)
    }

    /// X8': a fork with the child's own unmatched copy at `at` in its own part.
    fn cx_fork(own_before_task: &[Value], own_after_task: &[Value]) -> String {
        let child = "88888888-8888-4888-8888-888888888888";
        let parent = "99999999-9999-4999-8999-999999999999";
        let mut meta = cx_meta(0, child, "Sys.");
        meta["payload"]["forked_from_id"] = json!(parent);
        let mut r = vec![meta, cx_meta(0, parent, "Sys.")];
        r.extend(cx_turn(1, "t0"));
        r.push(cx_user(2, "Parent asks."));
        r.push(cx_event_said(3, "Parent answers.", "final_answer"));
        r.push(cx_said(3, "Parent answers.", Some("final_answer")));
        r.extend(cx_turn(4, "t1"));
        r.extend(own_before_task.iter().cloned());
        r.push(cx_user(5, "Child task."));
        r.extend(own_after_task.iter().cloned());
        r.push(cx::line(
            6,
            "response_item",
            json!({"type": "reasoning", "summary": [{"type": "summary_text", "text": "Child plan."}],
                "content": null, "encrypted_content": cx::REASONING}),
        ));
        r.push(cx::line(
            6,
            "event_msg",
            json!({"type": "agent_reasoning", "text": "Child plan."}),
        ));
        r.push(cx_said(7, "Done.", Some("final_answer")));
        jsonl(&r)
    }

    fn attacks() -> Vec<(&'static str, &'static str, Out)> {
        use crate::archive_v3::codex::import as cx_import;
        use crate::schemas::blockdag as main;
        let mut out: Vec<(&'static str, &'static str, Out)> = Vec::new();
        let s = source();
        let fresh = || import_ok(&s);

        // ---- controls: the same helpers, a break the gate must catch
        {
            let i = fresh();
            let again = one(&i, "Again.");
            let mut f = i.fragment;
            let fact = holds(&f, again)[0];
            add(&mut f, Trible::force(&fact, &UNKNOWN_ATTR, &gv(R4_C)));
            out.push((
                "C1",
                "control: fact gains an unknown attribute",
                gated(&s, &TABLE, f),
            ));
        }
        {
            let i = fresh();
            let mut f = i.fragment;
            let leaf = subjects_any(f.facts(), jt::string.id())[0];
            let old = of(f.facts(), leaf, jt::string.id());
            remove(&mut f, &old);
            let h = put_text(&mut f, "rm");
            add(&mut f, Trible::force(&leaf, &jt::string.id(), &h));
            out.push((
                "C2",
                "control: string leaf value changed",
                gated(&s, &TABLE, f),
            ));
        }
        {
            let i = fresh();
            let again = one(&i, "Again.");
            let mut f = i.fragment;
            let w = witness_of(&f, again)[0];
            add(&mut f, Trible::force(&w, &UNKNOWN_ATTR, &gv(R4_C)));
            out.push((
                "C3",
                "control: witness gains an unknown attribute",
                gated(&s, &TABLE, f),
            ));
        }
        // ---- JSON structure (Claude Code call input)
        // J1 an object field whose value node has no kind: an invented key.
        {
            let i = fresh();
            let mut f = i.fragment;
            let (_, root) = call_fact(&f);
            let name = put_text(&mut f, "sudo");
            add(
                &mut f,
                Trible::force(&root, &jt::object_field.id(), &gv(R4_A)),
            );
            add(
                &mut f,
                Trible::force(&R4_A, &jt::kind.id(), &gv(jt::kind_field)),
            );
            add(&mut f, Trible::force(&R4_A, &jt::field_name.id(), &name));
            add(
                &mut f,
                Trible::force(&R4_A, &jt::field_value.id(), &gv(R4_B)),
            );
            out.push((
                "J1",
                "structure gains a field whose value has no kind",
                gated(&s, &TABLE, f),
            ));
        }
        // J2 an object field whose value is a string node with no string.
        {
            let i = fresh();
            let mut f = i.fragment;
            let (_, root) = call_fact(&f);
            let name = put_text(&mut f, "sudo");
            add(
                &mut f,
                Trible::force(&root, &jt::object_field.id(), &gv(R4_A)),
            );
            add(
                &mut f,
                Trible::force(&R4_A, &jt::kind.id(), &gv(jt::kind_field)),
            );
            add(&mut f, Trible::force(&R4_A, &jt::field_name.id(), &name));
            add(
                &mut f,
                Trible::force(&R4_A, &jt::field_value.id(), &gv(R4_B)),
            );
            add(
                &mut f,
                Trible::force(&R4_B, &jt::kind.id(), &gv(jt::kind_string)),
            );
            out.push((
                "J2",
                "structure gains a field: string node without a string",
                gated(&s, &TABLE, f),
            ));
        }
        // J3 an array entry whose value has no kind: an invented element.
        {
            let sa = call_source(json!({"command": "ls", "args": ["-l"]}));
            match ok_or_baseline(import, &TABLE, &sa) {
                Err(b) => out.push(("J3", "array gains a kindless element", b)),
                Ok(i) => {
                    let mut f = i.fragment;
                    let array = subjects_with(f.facts(), jt::kind.id(), jt::kind_array)[0];
                    let mut raw = [0u8; 32];
                    raw[31] = 1;
                    let index: Inline<U256BE> = Inline::new(raw);
                    add(
                        &mut f,
                        Trible::force(&R4_A, &jt::array_parent.id(), &gv(array)),
                    );
                    add(&mut f, Trible::force(&R4_A, &jt::array_index.id(), &index));
                    add(
                        &mut f,
                        Trible::force(&R4_A, &jt::array_value.id(), &gv(R4_B)),
                    );
                    add(
                        &mut f,
                        Trible::force(&R4_A, &jt::kind.id(), &gv(jt::kind_array_entry)),
                    );
                    out.push((
                        "J3",
                        "array gains a kindless element",
                        gated(&sa, &TABLE, f),
                    ));
                }
            }
        }
        // J4 a string leaf gains an unknown attribute.
        {
            let i = fresh();
            let mut f = i.fragment;
            let leaf = subjects_any(f.facts(), jt::string.id())[0];
            add(&mut f, Trible::force(&leaf, &UNKNOWN_ATTR, &gv(R4_C)));
            out.push((
                "J4",
                "string leaf gains an unknown attribute",
                gated(&s, &TABLE, f),
            ));
        }
        // J5 a string leaf gains a number_raw value (A reads by kind only).
        {
            let i = fresh();
            let mut f = i.fragment;
            let leaf = subjects_any(f.facts(), jt::string.id())[0];
            let seven = put_text(&mut f, "7");
            add(&mut f, Trible::force(&leaf, &jt::number_raw.id(), &seven));
            out.push((
                "J5",
                "string leaf gains a number_raw value",
                gated(&s, &TABLE, f),
            ));
        }
        // J6 an object field entry gains an unknown attribute.
        {
            let i = fresh();
            let mut f = i.fragment;
            let entry = subjects_any(f.facts(), jt::field_name.id())[0];
            add(&mut f, Trible::force(&entry, &UNKNOWN_ATTR, &gv(R4_C)));
            out.push((
                "J6",
                "field entry gains an unknown attribute",
                gated(&s, &TABLE, f),
            ));
        }

        // ---- named entities (media type, vendor model)
        let ss = sealed_source();
        // N1 a fact's media type entity gains an unknown attribute.
        {
            let i = import_ok(&ss);
            let mut f = i.fragment;
            let (_, mt) = subjects_of(f.facts(), v3::fact::media_type.id())[0];
            add(&mut f, Trible::force(&mt, &UNKNOWN_ATTR, &gv(R4_C)));
            out.push((
                "N1",
                "media type entity gains an unknown attribute",
                gated(&ss, &TABLE, f),
            ));
        }
        // N2 a fact's media type entity loses its kind tag.
        {
            let i = import_ok(&ss);
            let mut f = i.fragment;
            let (_, mt) = subjects_of(f.facts(), v3::fact::media_type.id())[0];
            drop_tag(&mut f, mt, v3::media_type::KIND);
            out.push((
                "N2",
                "media type entity loses its kind tag",
                gated(&ss, &TABLE, f),
            ));
        }
        // N3 the vendor model entity gains an unknown attribute.
        {
            let i = fresh();
            let mut f = i.fragment;
            let (_, model) = subjects_of(f.facts(), v3::witness::generated_by.id())[0];
            add(&mut f, Trible::force(&model, &UNKNOWN_ATTR, &gv(R4_C)));
            out.push((
                "N3",
                "vendor model entity gains an unknown attribute",
                gated(&s, &TABLE, f),
            ));
        }
        // N4 the vendor model entity loses its tags.
        {
            let i = fresh();
            let mut f = i.fragment;
            let (_, model) = subjects_of(f.facts(), v3::witness::generated_by.id())[0];
            let tags = of(f.facts(), model, metadata::tag.id());
            assert!(!tags.is_empty());
            remove(&mut f, &tags);
            out.push((
                "N4",
                "vendor model entity loses its kind tag",
                gated(&s, &TABLE, f),
            ));
        }

        // ---- witnesses and occurrences
        let sg = regenerated_source();
        // W1 a bare second witness (no namespace) takes one of two occurrences.
        {
            let (i, hello, [_, late]) = regenerated();
            let mut f = i.fragment;
            add(
                &mut f,
                Trible::force(&R4_D, &v3::witness::projects_to.id(), &gv(hello)),
            );
            add(
                &mut f,
                Trible::force(&R4_D, &metadata::tag.id(), &gv(v3::witness::KIND)),
            );
            set_id(&mut f, late, v3::occurrence::of.id(), R4_D);
            out.push((
                "W1",
                "bare second witness takes an occurrence",
                gated(&sg, &TABLE, f),
            ));
        }
        // W2 a bare second witness alone on a witnessed node.
        {
            let i = fresh();
            let again = one(&i, "Again.");
            let mut f = i.fragment;
            add(
                &mut f,
                Trible::force(&R4_D, &v3::witness::projects_to.id(), &gv(again)),
            );
            add(
                &mut f,
                Trible::force(&R4_D, &metadata::tag.id(), &gv(v3::witness::KIND)),
            );
            out.push((
                "W2",
                "bare second witness on a witnessed node",
                gated(&s, &TABLE, f),
            ));
        }
        // W3 a bare occurrence (claims nothing) on a witness.
        {
            let i = fresh();
            let again = one(&i, "Again.");
            let mut f = i.fragment;
            let w = witness_of(&f, again)[0];
            add(
                &mut f,
                Trible::force(&R4_E, &v3::occurrence::of.id(), &gv(w)),
            );
            add(
                &mut f,
                Trible::force(&R4_E, &metadata::tag.id(), &gv(v3::occurrence::KIND)),
            );
            out.push((
                "W3",
                "bare occurrence (no claims) on a witness",
                gated(&s, &TABLE, f),
            ));
        }
        // W4 a witness gains a second, undecodable projects_to.
        {
            let i = fresh();
            let again = one(&i, "Again.");
            let mut f = i.fragment;
            let w = witness_of(&f, again)[0];
            add(
                &mut f,
                Trible::force(&w, &v3::witness::projects_to.id(), &garbage()),
            );
            out.push((
                "W4",
                "witness gains an undecodable projects_to",
                gated(&s, &TABLE, f),
            ));
        }
        // W5 an occurrence gains a second, undecodable of.
        {
            let i = fresh();
            let again = one(&i, "Again.");
            let mut f = i.fragment;
            let o = occ_of(&f, witness_of(&f, again)[0])[0];
            add(
                &mut f,
                Trible::force(&o, &v3::occurrence::of.id(), &garbage()),
            );
            out.push((
                "W5",
                "occurrence gains an undecodable of",
                gated(&s, &TABLE, f),
            ));
        }

        // ---- kind tags lost
        {
            let i = fresh();
            let again = one(&i, "Again.");
            let mut f = i.fragment;
            let fact = holds(&f, again)[0];
            drop_tag(&mut f, fact, v3::fact::KIND);
            out.push(("K1", "fact loses its kind tag", gated(&s, &TABLE, f)));
        }
        {
            let i = fresh();
            let again = one(&i, "Again.");
            let mut f = i.fragment;
            drop_tag(&mut f, again, v3::node::KIND);
            out.push(("K2", "node loses its kind tag", gated(&s, &TABLE, f)));
        }
        {
            let i = fresh();
            let again = one(&i, "Again.");
            let mut f = i.fragment;
            let w = witness_of(&f, again)[0];
            drop_tag(&mut f, w, v3::witness::KIND);
            out.push(("K3", "witness loses its kind tag", gated(&s, &TABLE, f)));
        }
        {
            let i = fresh();
            let again = one(&i, "Again.");
            let mut f = i.fragment;
            let o = occ_of(&f, witness_of(&f, again)[0])[0];
            drop_tag(&mut f, o, v3::occurrence::KIND);
            out.push(("K4", "occurrence loses its kind tag", gated(&s, &TABLE, f)));
        }
        {
            let i = fresh();
            let mut f = i.fragment;
            let (g, _) = gloss_on(&f, v3::gloss::category::TITLE);
            drop_tag(&mut f, g, v3::gloss::KIND);
            out.push(("K5", "title gloss loses its kind tag", gated(&s, &TABLE, f)));
        }

        // ---- structure links with undecodable values
        {
            let i = fresh();
            let again = one(&i, "Again.");
            let mut f = i.fragment;
            add(
                &mut f,
                Trible::force(&again, &v3::node::previous.id(), &garbage()),
            );
            out.push((
                "L1",
                "node gains an undecodable previous",
                gated(&s, &TABLE, f),
            ));
        }
        {
            let i = fresh();
            let mut f = i.fragment;
            let (g, _) = gloss_on(&f, v3::gloss::category::TITLE);
            add(
                &mut f,
                Trible::force(&g, &v3::gloss::describes.id(), &garbage()),
            );
            out.push((
                "L2",
                "gloss gains an undecodable describes",
                gated(&s, &TABLE, f),
            ));
        }

        // ---- facts per node
        // F1 the call fact split in two on its node.
        {
            let i = fresh();
            let mut f = i.fragment;
            let (fact, root) = call_fact(&f);
            let node = holder(&f, fact);
            for t in entity(f.facts(), fact) {
                if *t.a() != v3::fact::tool_name.id() {
                    add(&mut f, Trible::force(&R4_F, t.a(), t.v::<UnknownInline>()));
                }
            }
            let st = of(f.facts(), fact, v3::fact::structure.id());
            remove(&mut f, &st);
            let _ = root;
            add(
                &mut f,
                Trible::force(&node, &v3::node::holds.id(), &gv(R4_F)),
            );
            out.push((
                "F1",
                "call fact split into two facts on one node",
                gated(&s, &TABLE, f),
            ));
        }
        // F2 a fact duplicated under a second entity on its node.
        {
            let i = fresh();
            let again = one(&i, "Again.");
            let mut f = i.fragment;
            let fact = holds(&f, again)[0];
            clone_entity(&mut f, fact, R4_F);
            add(
                &mut f,
                Trible::force(&again, &v3::node::holds.id(), &gv(R4_F)),
            );
            out.push((
                "F2",
                "fact duplicated under a second entity on its node",
                gated(&s, &TABLE, f),
            ));
        }
        // F3 the whole witnessed node duplicated (fresh node, witness, occurrence).
        {
            let i = fresh();
            let again = one(&i, "Again.");
            let mut f = i.fragment;
            clone_entity(&mut f, again, R4_A);
            let w = witness_of(&f, again)[0];
            clone_entity(&mut f, w, R4_B);
            set_id(&mut f, R4_B, v3::witness::projects_to.id(), R4_A);
            let o = occ_of(&f, w)[0];
            clone_entity(&mut f, o, R4_C);
            set_id(&mut f, R4_C, v3::occurrence::of.id(), R4_B);
            out.push((
                "F3",
                "witnessed node duplicated with its witness and occurrence",
                gated(&s, &TABLE, f),
            ));
        }

        // ---- Codex glosses
        {
            let x = cx_two_summaries();
            match ok_or_baseline(cx_import, &CX, &x) {
                Err(b) => out.push(("G1", "two summary glosses merged into one entity", b)),
                Ok(i) => {
                    let mut f = i.fragment;
                    let gs: Vec<Id> = subjects_with(
                        f.facts(),
                        v3::gloss::category.id(),
                        v3::gloss::category::REASONING_SUMMARY,
                    );
                    assert_eq!(gs.len(), 2, "two glosses");
                    let text = of(f.facts(), gs[1], v3::gloss::text.id());
                    let all = entity(f.facts(), gs[1]);
                    remove(&mut f, &all);
                    add(
                        &mut f,
                        Trible::force(&gs[0], text[0].a(), text[0].v::<UnknownInline>()),
                    );
                    out.push((
                        "G1",
                        "two summary glosses merged into one entity",
                        gated(&x, &CX, f),
                    ));
                }
            }
        }
        // G2 Codex: the two summaries' texts swapped between... a single gloss
        // keeps both and the other is dropped is G1; here the second gloss
        // moves its text onto a fresh gloss entity on another node (her answer).
        // (placement; expected caught)

        // ---- importer refusals: backup-snapshot shapes (Claude Code)
        {
            let cut = format!(
                "{s}{}",
                "{\"type\":\"user\",\"uuid\":\"u9\",\"parentUuid\":\"a5\",\"mess"
            );
            out.push((
                "B1",
                "a truncated last line (backup cut mid-write)",
                fed(import, &TABLE, &cut),
            ));
        }
        {
            let mut lines: Vec<String> = s.lines().map(str::to_owned).collect();
            let changed = lines[2].replace("Again.", "Changed.");
            lines.push(changed);
            let twice = lines.join("\n") + "\n";
            out.push((
                "B2",
                "one uuid written twice with different text",
                fed(import, &TABLE, &twice),
            ));
        }
        {
            let mut lines: Vec<String> = s.lines().map(str::to_owned).collect();
            let mut v: Value = serde_json::from_str(&lines[1]).unwrap();
            v["message"]["stop_reason"] = json!("max_tokens");
            lines.push(serde_json::to_string(&v).unwrap());
            let twice = lines.join("\n") + "\n";
            out.push((
                "B3",
                "one uuid written twice, the second with another stop",
                fed(import, &TABLE, &twice),
            ));
        }
        {
            let nul = s.replacen("\n", "\n\u{0}\n", 1);
            out.push((
                "B4",
                "a NUL line (sparse backup tail) mid-file",
                fed(import, &TABLE, &nul),
            ));
        }

        // ---- importer refusals: Codex supply, guardian, errors, summaries
        {
            let mut r = cx_head();
            r.push(cx_user(2, "Q."));
            r.push(cx_said(3, "Looking.", None));
            r.push(cx_event_said(3, "Looking.", "commentary"));
            out.push((
                "S1",
                "phase copy after its message (text match only)",
                fed(cx_import, &CX, &jsonl(&r)),
            ));
        }
        {
            let mut r = cx_head();
            r.push(cx_user(2, "Q."));
            r.push(cx_event_said(3, "Looking.", "commentary"));
            r.push(cx::line(
                3,
                "response_item",
                json!({"type": "function_call", "name": "ls",
                "arguments": "{}", "call_id": "c1"}),
            ));
            r.push(cx::line(
                3,
                "response_item",
                json!({"type": "function_call_output",
                "call_id": "c1", "output": "x"}),
            ));
            r.push(cx_said(4, "Looking.", None));
            out.push((
                "S2",
                "phase copy separated from its message by a call",
                fed(cx_import, &CX, &jsonl(&r)),
            ));
        }
        {
            let mut r = cx_head();
            r.push(cx_user(2, "Q."));
            r.push(cx_event_said(3, "Same.", "commentary"));
            r.push(cx_said(3, "Other.", None));
            r.push(cx_said(4, "Same.", None));
            out.push((
                "S3",
                "phase copy before another message, its text held later",
                fed(cx_import, &CX, &jsonl(&r)),
            ));
        }
        {
            let ask = json!({"type": "message", "role": "user",
                "content": [{"type": "input_text", "text": "A."}]});
            out.push((
                "H1",
                "guardian_history holds one item twice",
                fed(cx_import, &CX, &cx_guardian(json!([ask, ask]))),
            ));
        }
        {
            let event = json!({"type": "user_message", "message": "A.",
                "images": [], "local_images": [], "text_elements": []});
            out.push((
                "H2",
                "guardian_history holds an event payload, not an item",
                fed(cx_import, &CX, &cx_guardian(json!([event]))),
            ));
        }
        {
            let comp = json!({"type": "compaction", "encrypted_content": cx::COMPACTION});
            out.push((
                "H3",
                "guardian_history holds the replacement's own compaction item",
                fed(cx_import, &CX, &cx_guardian(json!([comp]))),
            ));
        }
        {
            let mut v: Vec<Value> = cx::g2()
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect();
            v[15]["payload"]["error"] = json!({});
            out.push((
                "E1",
                "task_complete.error an empty object",
                fed(cx_import, &CX, &jsonl(&v)),
            ));
        }
        {
            let mut v: Vec<Value> = cx::g2()
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect();
            v[15]["payload"]["error"] = json!({"message": null, "codex_error_info": "other"});
            out.push((
                "E2",
                "task_complete.error message null",
                fed(cx_import, &CX, &jsonl(&v)),
            ));
        }
        {
            let mut v: Vec<Value> = cx::g2()
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect();
            v[6]["payload"]["summary"] = json!([{"type": "summary_text", "text": null}]);
            out.push((
                "E3",
                "summary part text null",
                fed(cx_import, &CX, &jsonl(&v)),
            ));
        }
        {
            let mut v: Vec<Value> = cx::g2()
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect();
            v[6]["payload"]["summary"] = json!([{"type": "summary_text", "text": ""}]);
            out.push((
                "E4",
                "summary part text empty",
                fed(cx_import, &CX, &jsonl(&v)),
            ));
        }
        {
            let mut v: Vec<Value> = cx::g2()
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect();
            v[4]["payload"]["content"] = json!([{"type": "input_text", "text": ""}]);
            out.push((
                "E5",
                "her prompt part text empty",
                fed(cx_import, &CX, &jsonl(&v)),
            ));
        }
        {
            let thought = cx::line(
                5,
                "event_msg",
                json!({"type": "agent_reasoning", "text": "Child's own aside."}),
            );
            out.push((
                "X1",
                "fork: child's unmatched reasoning copy after its own prompt",
                fed(cx_import, &CX, &cx_fork(&[], &[thought])),
            ));
        }
        {
            let thought = cx::line(
                4,
                "event_msg",
                json!({"type": "agent_reasoning", "text": "Child's own aside."}),
            );
            out.push((
                "X2",
                "fork: child's unmatched copy after its own task_started",
                fed(cx_import, &CX, &cx_fork(&[thought], &[])),
            ));
        }
        {
            let comment = cx_event_said(5, "Child comments.", "commentary");
            out.push((
                "X3",
                "fork: child's unmatched commentary copy after its own prompt",
                fed(cx_import, &CX, &cx_fork(&[], &[comment])),
            ));
        }

        // ---- report values
        {
            let sv = call_source(json!({"command": "ls", "hunter2key": "Bremen-secret"}));
            match ok_or_baseline(import, &TABLE, &sv) {
                Err(b) => out.push(("V1", "structure key named by a secret, dropped by A", b)),
                Ok(i) => {
                    let (_, root) = call_fact(&i.fragment);
                    let entry = field_entry(&i, root, "hunter2key");
                    let mut f = i.fragment;
                    let t = Trible::force(&root, &jt::object_field.id(), &gv(entry));
                    remove(&mut f, &[t]);
                    out.push((
                        "V1",
                        "structure key named by a secret, dropped by A",
                        gated(&sv, &TABLE, f),
                    ));
                }
            }
        }
        {
            let mut a1: Value = serde_json::from_str(s.lines().nth(1).unwrap()).unwrap();
            a1["message"]["usage"]["a$Bremen"] = json!(1);
            let mut lines: Vec<String> = s.lines().map(str::to_owned).collect();
            lines[1] = serde_json::to_string(&a1).unwrap();
            let mutated = lines.join("\n") + "\n";
            out.push((
                "V2",
                "printable unknown key with a secret (nested)",
                gated(&mutated, &TABLE, import_ok(&s).fragment),
            ));
            out.push((
                "V2i",
                "same, importer failure path",
                fed(import, &TABLE, &mutated),
            ));
        }
        {
            let mut v: Vec<Value> = cx::g2()
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect();
            v[3]["payload"]["sandbox_policy"]["Bremen_x"] = json!(true);
            let mutated = jsonl(&v);
            let x = cx::g2();
            let base = import_with(cx_import, &x);
            out.push((
                "V3",
                "Codex nested unknown key with a secret",
                gated(&mutated, &CX, base.fragment),
            ));
            out.push((
                "V3i",
                "same, importer failure path",
                fed(cx_import, &CX, &mutated),
            ));
        }
        {
            let mut v: Vec<Value> = cx::g2()
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect();
            v[1]["payload"]["role"] = json!("hunter2");
            let mutated = jsonl(&v);
            out.push((
                "V4",
                "Codex discriminator value a secret (role)",
                fed(cx_import, &CX, &mutated),
            ));
        }
        let _ = main::block::KIND;
        out
    }

    /// Cases the rulings accept as no defect (rulings-r5: the X8' fork-boundary
    /// cases, and unknown keys inside a subtree that is dropped whole). They
    /// are run, and must neither crash nor print a value.
    const ACCEPTED: &[&str] = &["X1", "X2", "X3", "V2", "V2i", "V3", "V3i"];
    /// Cases an importer ruling of round 5 owns, not the gate: X15 (an error
    /// whose `message` is no string fails). Asserted in their own test.
    const IMPORTER: &[&str] = &["E2"];

    #[test]
    fn the_gate_catches_every_round4_attack() {
        let cases = attacks();
        assert_eq!(cases.len(), 53);
        let mut missed = Vec::new();
        for (code, name, outcome) in &cases {
            match outcome {
                Out::Gated(report) => {
                    let text = format!("{report}");
                    assert!(leaks(&text).is_empty(), "{code} printed a value:\n{text}");
                    if report.passed() {
                        println!("R4\t{code}\tpassed\t{name}");
                        if !ACCEPTED.contains(code) && !IMPORTER.contains(code) {
                            missed.push(*code);
                        }
                    } else {
                        let mut kinds: Vec<String> = report
                            .rows
                            .iter()
                            .map(|r| format!("{}:{}", r.kind, r.path))
                            .collect();
                        kinds.sort();
                        kinds.dedup();
                        println!("R4\t{code}\tcaught\t{name}\t{}", kinds.join(" | "));
                    }
                }
                Out::Refused(path) => println!("R4\t{code}\trefused\t{name}\t{path}"),
                Out::Crashed(why) => panic!("{code} crashed: {why}"),
                Out::Baseline(why) => panic!("{code} has no right baseline: {why}"),
            }
        }
        assert!(
            missed.is_empty(),
            "the gate missed {} of round 4: {missed:?}",
            missed.len()
        );
    }

    /// X15: an erred turn's `message` must be a JSON string; `null` fails.
    /// The Codex importer applies it, not the gate.
    #[test]
    fn x15_an_error_message_that_is_no_string_fails() {
        let cases = attacks();
        let (_, _, outcome) = cases.iter().find(|(code, _, _)| *code == "E2").unwrap();
        match outcome {
            Out::Refused(_) => {}
            Out::Gated(report) => assert!(!report.passed(), "E2 passed the gate"),
            Out::Crashed(why) | Out::Baseline(why) => panic!("E2: {why}"),
        }
    }
}
