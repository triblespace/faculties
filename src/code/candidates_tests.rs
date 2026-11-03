use std::fs;
use std::path::{Path, PathBuf};

use super::*;
use crate::code::operations::{IngestOptions, Revision};
use crate::schemas::code::DEFAULT_SCOPE_ID;
use crate::storage::initialize_signer;
use triblespace::core::collection::CollectionStoreExt;

#[path = "../../tests/support/code_git.rs"]
mod code_git;

struct Fixture {
    directory: tempfile::TempDir,
    code: Code,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let pile = directory.path().join("code.pile");
        let key = directory.path().join("code.key");
        fs::File::create(&pile).unwrap();
        initialize_signer(&pile, Some(&key)).unwrap();
        Self {
            directory,
            code: Code::new(pile, Some(key)),
        }
    }

    fn repo(&self, name: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir = self.directory.path().join(name);
        fs::create_dir_all(&dir).unwrap();
        for (name, text) in files {
            fs::write(dir.join(name), text).unwrap();
        }
        code_git::initialize(&dir);
        self.code
            .ingest(&[dir.clone()], &IngestOptions::default())
            .unwrap();
        dir
    }

    fn item(&self, name: &str) -> Id {
        self.code.find(name, &Filter::default()).unwrap().hits[0].item
    }
}

fn commit(dir: &Path) -> String {
    let repo = git2::Repository::open(dir).unwrap();
    let mut index = repo.index().unwrap();
    index
        .add_all(["."], git2::IndexAddOption::DEFAULT, None)
        .unwrap();
    index.write().unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let parent = repo.head().unwrap().peel_to_commit().unwrap();
    let signature = git2::Signature::new(
        "fixture",
        "fixture@example.invalid",
        &git2::Time::new(7200, 0),
    )
    .unwrap();
    repo.commit(
        Some("HEAD"),
        &signature,
        &signature,
        "second",
        &tree,
        &[&parent],
    )
    .unwrap()
    .to_string()
}

const SEED: &str = "pub fn seed() { sensor::pulse(\"stellar telemetry pulse\"); }\n";

#[test]
fn candidates_keep_independent_reasons_exact_places_and_literal_source() {
    let fixture = Fixture::new();
    fixture.repo("consumer", &[("seed.rs", SEED), ("copy.rs", SEED)]);
    fixture.repo(
        "helpers",
        &[
            (
                "prose.rs",
                "/// Stellar telemetry pulse.\npub fn describe() {}\n",
            ),
            ("calls.rs", "pub fn emit() { sensor::pulse(); }\n"),
        ],
    );
    fixture.code.index().unwrap();
    let seed = fixture.item("seed");
    let report = fixture
        .code
        .candidates(seed, 20, true, &Filter::default())
        .unwrap();
    assert_eq!(report.exact_placements.len(), 2);
    assert!(report.candidates.iter().all(|row| row.item != seed));
    let prose = report
        .candidates
        .iter()
        .find(|row| row.places[0].name.as_deref() == Some("describe"))
        .unwrap();
    assert!(prose.lexical.doc > 0.0);
    assert!(prose.shared_mentions.is_empty(), "{prose:?}");
    let calls = report
        .candidates
        .iter()
        .find(|row| row.places[0].name.as_deref() == Some("emit"))
        .unwrap();
    assert!(calls.lexical.source_tokens > 0.0);
    assert!(calls
        .shared_mentions
        .iter()
        .any(|reason| reason.spelling.as_deref() == Some("sensor::pulse")));
    assert!(calls.source.as_ref().unwrap().contains("sensor::pulse();"));
    assert_eq!(calls.places[0].line, 1);
    assert_eq!(calls.places[0].end_line, 1);
    assert!(report.candidates.len() <= 40);
}

#[test]
fn stored_mentions_drive_retrieval_even_when_not_in_the_source_tokens() {
    let fixture = Fixture::new();
    fixture.repo(
        "consumer",
        &[("seed.rs", SEED), ("other.rs", "pub fn annotated() {}\n")],
    );
    let seed = fixture.item("seed");
    let other = fixture.item("annotated");
    let mention = super::super::text_handle("later_extractor_annotation");
    fixture
        .code
        .storage()
        .with_store(|store, signer, runtime| {
            let collection = fixture.code.storage().open_collection_write(
                store,
                DEFAULT_SCOPE_ID,
                signer.verifying_key(),
                runtime,
            )?;
            let mut fragment = Fragment::empty();
            fragment += entity! { ExclusiveId::force_ref(&seed) @ attrs::mentions: mention };
            fragment += entity! { ExclusiveId::force_ref(&other) @ attrs::mentions: mention };
            store.commit(collection, signer, fragment)?;
            Ok(())
        })
        .unwrap();
    fixture.code.index().unwrap();
    let report = fixture
        .code
        .candidates(seed, 1, false, &Filter::default())
        .unwrap();
    let row = report
        .candidates
        .iter()
        .find(|row| row.item == other)
        .unwrap();
    let reason = row
        .shared_mentions
        .iter()
        .find(|reason| reason.handle == mention)
        .unwrap();
    assert_eq!(reason.spelling, None);
    assert_eq!(reason.items, 1);
}

#[test]
fn repo_and_historical_scope_precede_both_channel_limits() {
    let fixture = Fixture::new();
    fixture.repo("consumer", &[("seed.rs", SEED)]);
    let seed = fixture.item("seed");
    let noise: String = (0..120)
        .map(|n| {
            format!("/// Stellar telemetry pulse.\npub fn noise_{n}() {{ sensor::pulse(); }}\n")
        })
        .collect();
    fixture.repo("noise", &[("noise.rs", &noise)]);
    let target = fixture.repo(
        "target",
        &[("helper.rs", "pub fn previous() { sensor::pulse(); }\n")],
    );
    let old = git2::Repository::open(&target)
        .unwrap()
        .head()
        .unwrap()
        .target()
        .unwrap()
        .to_string();
    let previous = fixture.item("previous");
    fs::write(
        target.join("helper.rs"),
        "pub fn current() { sensor::pulse(); }\n",
    )
    .unwrap();
    let new = commit(&target);
    fixture
        .code
        .ingest(&[target], &IngestOptions::default())
        .unwrap();
    fixture.code.index().unwrap();
    for (revision, name, item) in [
        (old, "previous", previous),
        (new, "current", fixture.item("current")),
    ] {
        let filter = Filter {
            repo: Some("target".to_owned()),
            kind: Some("fn".to_owned()),
            revision: Revision::Selector(revision.clone()),
        };
        let report = fixture.code.candidates(seed, 1, false, &filter).unwrap();
        assert_eq!(report.lexical_items, 1, "{report:?}");
        assert_eq!(report.mention_items, 1);
        assert!(report.exact_placements.is_empty());
        assert_eq!(report.candidates.len(), 1);
        let row = &report.candidates[0];
        assert_eq!(row.item, item);
        assert_eq!(row.lexical_rank, Some(1));
        assert_eq!(row.mention_rank, Some(1));
        assert_eq!(row.places[0].name.as_deref(), Some(name));
        assert!(row
            .places
            .iter()
            .all(|place| place.repo == "target" && place.scan.contains(&revision[..8])));
        assert!(row.shared_mentions.iter().all(|reason| reason.items == 1));
    }
}

#[test]
fn cli_and_mcp_share_candidate_scope_and_source_output() {
    use crate::code::{cli, mcp};
    use crate::mcp::Faculty;
    use crate::out::{Out, Part};
    use anybytes::Bytes;
    use clap::Parser;
    let fixture = Fixture::new();
    fixture.repo(
        "consumer",
        &[
            ("seed.rs", SEED),
            ("calls.rs", "pub fn emit() { sensor::pulse(); }\n"),
        ],
    );
    fixture.code.index().unwrap();
    let item = format!("{:x}", fixture.item("seed"));
    let pile = fixture.directory.path().join("code.pile");
    let key = fixture.directory.path().join("code.key");
    let cli = cli::Cli::try_parse_from([
        "code",
        "--pile",
        pile.to_str().unwrap(),
        "--key",
        key.to_str().unwrap(),
        "candidates",
        &item,
        "--top",
        "2",
        "--source",
        "--kind",
        "fn",
    ])
    .unwrap();
    let mut cli_text = String::new();
    cli::execute(
        cli,
        &mut Out::new(&mut |part| {
            if let Part::Text { text } = part {
                cli_text.push_str(&text);
            }
            Ok(())
        }),
    )
    .unwrap();
    let args = serde_json::to_vec(&serde_json::json!({
        "item": item, "top": 2, "source": true, "kind": "fn",
    }))
    .unwrap();
    let mut mcp_text = String::new();
    mcp::Code::new(pile, Some(key))
        .call(
            "code_candidates",
            Bytes::from_source(args),
            &mut Out::new(&mut |part| {
                if let Part::Text { text } = part {
                    mcp_text.push_str(&text);
                }
                Ok(())
            }),
        )
        .unwrap();
    assert_eq!(cli_text, mcp_text);
    assert!(cli_text.contains("source from first placement"));
    assert!(cli_text.contains("sensor::pulse();"));
    assert!(cli_text.contains("Candidate evidence only"));
}
