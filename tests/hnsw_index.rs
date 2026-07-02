//! Faculty-level proof for the persisted HNSW index-home path.
//!
//! Stages synthetic 768-d embeddings DIRECTLY under entity ids on a fresh
//! temporary pile (no nomic model), refreshes the HNSW segment via
//! [`embeddings::update_index`], then queries via
//! [`embeddings::nearest_via_index`] — the same fast path `memory similar` /
//! `wiki similar` now take. Proves the query attaches the persisted segment(s)
//! and returns the staged nearest neighbour WITHOUT a full checkout + rebuild.

use std::path::PathBuf;

use ed25519_dalek::SigningKey;
use rand_core::OsRng;

use faculties::schemas::embeddings::{self, Embedding768, DIM};
use triblespace::core::repo::pile::Pile;
use triblespace::core::repo::Repository;
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
    dir.join(format!("faculties-hnsw-test-{}-{}.pile", std::process::id(), nanos))
}

/// Deterministic pseudo-random L2-normalized vector, tagged by `seed`.
fn unit_vec(seed: u64) -> Vec<f32> {
    let mut rng = seed.wrapping_mul(0x9E3779B97F4A7C15).wrapping_add(1);
    let mut next = || {
        rng = rng.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = rng;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        (z ^ (z >> 31)) as i64 as f32 / i64::MAX as f32
    };
    let mut v: Vec<f32> = (0..DIM).map(|_| next()).collect();
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        for x in &mut v {
            *x /= n;
        }
    }
    v
}

#[test]
fn similar_via_persisted_index_returns_staged_nearest() {
    let path = temp_pile_path();

    // ── build a fresh pile with staged embeddings ──────────────────────────
    let branch_id;
    let ids: Vec<Id> = (0..24).map(|_| *fucid()).collect();
    let vecs: Vec<Vec<f32>> = (0..ids.len() as u64).map(unit_vec).collect();
    {
        std::fs::File::create(&path).expect("create empty pile file");
        let pile = Pile::open(&path).expect("open temp pile");
        let mut repo = Repository::new(pile, SigningKey::generate(&mut OsRng), TribleSet::new())
            .expect("create repo");
        branch_id = *repo.create_branch("main", None).expect("branch");

        let mut ws = repo.pull(branch_id).expect("pull");
        let mut change = TribleSet::new();
        for (id, v) in ids.iter().zip(&vecs) {
            let handle = ws.put::<Embedding768, _>(v.clone());
            change += entity! { ExclusiveId::force_ref(id) @ embeddings::attr::embedding: handle };
        }
        let delta = change.clone();
        ws.commit(change, "stage synthetic embeddings");
        repo.push(&mut ws).expect("push");

        // Refresh the persisted HNSW segment — the maintenance step `memory
        // embed` / `wiki embed` now run.
        embeddings::update_index(repo.storage_mut(), branch_id, &delta)
            .expect("update hnsw index");
        repo.close().expect("close");
    }

    // ── reopen and query via the persisted segment (no rebuild) ────────────
    {
        let pile = Pile::open(&path).expect("reopen temp pile");
        let mut repo = Repository::new(pile, SigningKey::generate(&mut OsRng), TribleSet::new())
            .expect("reopen repo");

        // Probe with entity 7's own vector: its own handle must rank first.
        let probe_idx = 7usize;
        let rows = embeddings::nearest_via_index(repo.storage_mut(), branch_id, &vecs[probe_idx], 0.0)
            .expect("query")
            .expect("a persisted HNSW segment exists");
        assert!(!rows.is_empty(), "segment query returned candidates");

        // Map the top handle back to its entity via the staged tribles.
        let mut ws = repo.pull(branch_id).expect("pull");
        let space = ws.checkout(..).expect("checkout");
        let mut by_handle: std::collections::HashMap<[u8; 32], Id> =
            std::collections::HashMap::new();
        for (id, v) in ids.iter().zip(&vecs) {
            let h: Inline<inlineencodings::Handle<Embedding768>> = find!(
                h: Inline<inlineencodings::Handle<Embedding768>>,
                pattern!(&space, [{ *id @ embeddings::attr::embedding: ?h }])
            )
            .next()
            .unwrap_or_else(|| panic!("embedding trible for {id:x} present"));
            let _ = v;
            by_handle.insert(h.raw, *id);
        }

        let (top_cos, top_raw) = rows[0];
        let top_id = by_handle
            .get(&top_raw)
            .copied()
            .expect("top handle maps to a staged entity");
        assert_eq!(top_id, ids[probe_idx], "self is the nearest neighbour");
        assert!(top_cos > 0.99, "self-cosine ~1.0, got {top_cos}");

        repo.close().expect("close");
    }

    let _ = std::fs::remove_file(&path);
}
