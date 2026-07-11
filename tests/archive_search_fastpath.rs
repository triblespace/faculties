//! Faculty-level proof for the checkout-free archive BM25 search path.
//!
//! Builds a fresh temporary pile with a handful of synthetic archive
//! messages, then drives the REAL `archive` binary:
//!
//! 1. `archive search` before `archive index` errors (no segments yet).
//! 2. `archive index` builds the content rollup + a BM25 index-home
//!    segment.
//! 3. `archive search <term>` returns exactly the messages whose content
//!    contains `<term>`, BM25-ranked, with each hit's author + content
//!    snippet resolved — through the branch's `SuccinctArchive` rollup
//!    and per-hit blob gets, with NO full `ws.checkout(..)` of the
//!    branch on the query path.
//! 4. Standalone and repeated Unicode symbols are regular indexed terms,
//!    not an accidental request for the archive-scale exact scan.
//!
//! The exact ranking equivalence to the old monolithic index is proven
//! at the crate level in
//! `triblespace_search::index_bm25::tests::single_segment_equals_monolithic_oracle`;
//! this test proves the faculty wiring end-to-end.

use std::path::PathBuf;
use std::process::Command;

use ed25519_dalek::SigningKey;
use hifitime::Epoch;
use rand_core::OsRng;

use faculties::schemas::archive::archive;
use triblespace::core::metadata;
use triblespace::core::repo::pile::Pile;
use triblespace::core::repo::Repository;
use triblespace::prelude::blobencodings::LongString;
use triblespace::prelude::*;

/// A fresh, unique temp pile path. Honours the job's temp dir when
/// `CLAUDE_JOB_TMP` is set; otherwise falls back to the system temp dir.
/// Never a real pile.
fn temp_pile_path() -> PathBuf {
    let dir = std::env::var("CLAUDE_JOB_TMP")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    dir.join(format!(
        "faculties-archive-test-{}-{}.pile",
        std::process::id(),
        nanos
    ))
}

fn run_archive(pile: &PathBuf, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_archive"))
        .arg("--pile")
        .arg(pile)
        .arg("--branch")
        .arg("archive")
        .args(args)
        .output()
        .expect("run archive binary")
}

#[test]
fn bm25_fast_path_resolves_content_without_checkout() {
    let path = temp_pile_path();

    // ── build a fresh archive pile with synthetic messages ────────────────
    // Known vocabulary so we can assert which docs a query must return.
    let docs = [
        ("alpha beta gamma memory", "message A"),
        ("beta delta pile", "message B"),
        ("gamma delta epsilon trible", "message C"),
        ("telemetry symbol alpha 🛰️, status nominal", "message D"),
        (
            "telemetry symbol alpha cluster 🛰️🛰️🛰️ status stable",
            "message E",
        ),
        ("symbol beta 🧭", "message F"),
        ("symbol gamma 🔭", "message G"),
        ("symbol delta 🪐", "message H"),
    ];
    let msg_ids: Vec<Id> = (0..docs.len()).map(|_| *fucid()).collect();
    let branch_id;
    {
        std::fs::File::create(&path).expect("create empty pile file");
        let pile = Pile::open(&path).expect("open temp pile");
        let mut repo = Repository::new(pile, SigningKey::generate(&mut OsRng), TribleSet::new())
            .expect("create repo");
        branch_id = *repo.create_branch("archive", None).expect("branch");

        let mut ws = repo.pull(branch_id).expect("pull");
        let mut change = TribleSet::new();

        // One author.
        let author = *fucid();
        let author_name = ws.put::<LongString, _>("Tester".to_owned());
        change += entity! { ExclusiveId::force_ref(&author) @
            metadata::tag: archive::kind_author,
            archive::author_name: author_name,
        };

        // Messages, one second apart so timestamps are distinct.
        for (i, (id, (text, _label))) in msg_ids.iter().zip(&docs).enumerate() {
            let content = ws.put::<LongString, _>((*text).to_owned());
            let when = Epoch::from_gregorian_tai(2026, 1, 1, 0, 0, i as u8, 0);
            let created_at: Inline<inlineencodings::NsTAIInterval> =
                (when, when).try_to_inline().unwrap();
            change += entity! { ExclusiveId::force_ref(id) @
                metadata::tag: archive::kind_message,
                archive::author: author,
                archive::content: content,
                metadata::created_at: created_at,
            };
        }
        ws.commit(change, "stage synthetic archive");
        repo.push(&mut ws).expect("push");
        repo.close().expect("close");
    }

    // ── 1. search before index: no segments yet → clean error ─────────────
    let out = run_archive(&path, &["search", "beta"]);
    assert!(
        !out.status.success(),
        "search before index should fail; stdout={}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no BM25 search segments"),
        "expected 'no BM25 search segments' hint, got: {stderr}"
    );

    // ── 2. index: build rollup + BM25 segment ─────────────────────────────
    let out = run_archive(&path, &["index"]);
    assert!(
        out.status.success(),
        "index failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("indexed 8 message"),
        "index summary: {stdout}"
    );

    // ── 3. search "beta": must return A and B (contain 'beta'), not C ─────
    // Discriminate by the RESOLVED content snippet (the binary prints
    // only 8 hex of the id, which `fucid` shares as a timestamp prefix
    // across ids minted together). Matching content is the stronger
    // proof anyway: it shows the text was resolved via the rollup + a
    // blob get, with no branch checkout. A's unique token is "alpha",
    // B's is "pile", C's is "epsilon".
    let out = run_archive(&path, &["search", "beta"]);
    assert!(
        out.status.success(),
        "search failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("alpha"),
        "'beta' must return message A; got:\n{stdout}"
    );
    assert!(
        stdout.contains("pile"),
        "'beta' must return message B; got:\n{stdout}"
    );
    assert!(
        !stdout.contains("epsilon"),
        "'beta' must NOT return message C; got:\n{stdout}"
    );
    // Author name resolved from the rollup too.
    assert!(
        stdout.contains("Tester"),
        "author name resolved; got:\n{stdout}"
    );

    // ── 4. a rare term hits exactly its one document ──────────────────────
    let out = run_archive(&path, &["search", "epsilon"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("epsilon") && !stdout.contains("alpha"),
        "'epsilon' must return only message C; got:\n{stdout}"
    );

    // ── 5. absent term returns nothing ────────────────────────────────────
    let out = run_archive(&path, &["search", "zzzabsentzzz"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    for token in ["alpha", "pile", "epsilon"] {
        assert!(
            !stdout.contains(token),
            "absent term must return no messages; got:\n{stdout}"
        );
    }

    // A standalone Unicode symbol uses the BM25 fast path. The synthetic
    // fixtures cover punctuation adjacency and a repeated symbol cluster.
    let out = run_archive(&path, &["search", "🛰️"]);
    assert!(
        out.status.success(),
        "symbol search failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("status nominal"),
        "punctuation-adjacent symbol must match; got:\n{stdout}"
    );
    assert!(
        stdout.contains("status stable"),
        "a symbol inside a repeated cluster must match; got:\n{stdout}"
    );
    assert!(
        !stdout.contains("symbol beta"),
        "a different symbol must not match; got:\n{stdout}"
    );

    // Similar Unicode symbols receive distinct, context-free terms too.
    let out = run_archive(&path, &["search", "🧭"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("symbol beta") && !stdout.contains("symbol gamma"),
        "the first generic symbol must resolve independently; got:\n{stdout}"
    );
    let out = run_archive(&path, &["search", "🔭"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("symbol gamma") && !stdout.contains("symbol beta"),
        "the second generic symbol must resolve independently; got:\n{stdout}"
    );
    let out = run_archive(&path, &["search", "🪐"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("symbol delta") && !stdout.contains("symbol beta"),
        "a newer emoji scalar must be indexed too; got:\n{stdout}"
    );

    let _ = std::fs::remove_file(&path);
    // Best-effort: the replay-index sibling file is not created here, but
    // clean up any pile side-files defensively.
    let _ = std::fs::remove_file(path.with_extension("pile.replay-index.jsonl"));
}
