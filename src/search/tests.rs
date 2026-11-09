//! Disposable local piles only: no ambient routes, models, network or GPU.
use super::*;
use crate::mcp::Faculty;
use crate::out::{Out, Part};
use crate::schemas::{config::config, files::file, memory::ctx, wiki, workspace};
use crate::storage::{CollectionRoutes, Storage};
use clap::Parser;
use std::collections::{BTreeMap, BTreeSet};
use triblespace::core::collection::{AdmissionPolicy, CollectionPolicy, CollectionStoreExt};
use triblespace::core::metadata;
use triblespace::core::repo::SnapshotSource;
use triblespace::prelude::*;

const WIKI: Id = crate::schemas::wiki::DEFAULT_SCOPE_ID;
const MEMORY: Id = crate::schemas::memory::DEFAULT_SCOPE_ID;
const FILES: Id = crate::schemas::files::DEFAULT_SCOPE_ID;

struct Fixture {
    directory: tempfile::TempDir,
    owner: Storage,
    selected: Storage,
    routes: CollectionRoutes,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let pile = directory.path().join("search.pile");
        let key = directory.path().join("search.key");
        std::fs::File::create(&pile).unwrap();
        crate::storage::initialize_signer(&pile, Some(&key)).unwrap();
        let owner = Storage::shared_local(pile, Some(key));
        let routes = owner
            .with_store(|store, signer, _| {
                [WIKI, MEMORY, FILES]
                    .into_iter()
                    .map(|scope| {
                        let collection = store.collection(
                            crate::collection_names::require_name(scope),
                            crate::collection_names::private_policy(signer.verifying_key()),
                        )?;
                        Ok((scope, collection.handle()))
                    })
                    .collect::<Result<CollectionRoutes>>()
            })
            .unwrap();
        let selected = owner.clone().with_collections(routes.clone());
        Self {
            directory,
            owner,
            selected,
            routes,
        }
    }

    fn publish(&self, scope: Id, fragment: Fragment) {
        self.selected
            .with_store(|store, signer, runtime| {
                let source = self.selected.open_collection_write(
                    store,
                    scope,
                    signer.verifying_key(),
                    runtime,
                )?;
                store.commit(source, signer, fragment)?;
                Ok(())
            })
            .unwrap();
    }

    fn wiki(&self, title: &str, body: &str, predecessors: &[Id]) -> Id {
        let fragment = wiki::revision_fragment(genid().id, title, body, &[], predecessors);
        let revision = fragment.root().unwrap();
        self.publish(WIKI, fragment);
        revision
    }

    fn memory(&self, text: &str) -> Id {
        let fragment = entity! { metadata::tag: crate::schemas::memory::KIND_CHUNK_ID,
        ctx::summary: text.to_owned() };
        let chunk = fragment.root().unwrap();
        self.publish(MEMORY, fragment);
        chunk
    }

    fn file(&self, bytes: &[u8], name: &str) -> Id {
        let fragment = crate::files::stage(bytes.to_vec(), name, "text/plain").unwrap();
        let holder = fragment.root().unwrap();
        self.publish(FILES, fragment);
        holder
    }

    fn pin(&self) -> Storage {
        self.owner
            .with_store(|store, signer, _| {
                let mut routes = Fragment::empty();
                for (&scope, &handle) in &self.routes {
                    routes += entity! { config::anchor: scope, config::selects: handle };
                }
                let revision = entity! {
                    metadata::tag: workspace::WORKSPACE,
                    config::anchor: genid().id,
                    workspace::enrolled: true,
                    metadata::name: "Personal search fixture",
                    workspace::workspace_route*: routes,
                };
                let id = revision.root().unwrap();
                let config = store.collection(
                    "config",
                    crate::collection_names::private_policy(signer.verifying_key()),
                )?;
                store.commit(config, signer, revision)?;
                let workspace =
                    crate::workspace::resolve_in(&store.snapshot()?, signer.verifying_key(), id)?
                        .expect("published workspace");
                Ok(self
                    .owner
                    .clone()
                    .with_workspace(workspace, signer.verifying_key()))
            })
            .unwrap()
    }

    fn query(&self, query: &str, limit: usize) -> Report {
        Search::with_storage(self.selected.clone())
            .query(Options {
                query,
                limit_per_source: limit,
            })
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.owner.close().unwrap();
    }
}

fn answered(source: &Source) -> (&[Hit], usize, bool) {
    match &source.outcome {
        Outcome::Answered {
            hits,
            total_matches,
            truncated,
        } => (hits, *total_matches, *truncated),
        Outcome::Unavailable { message } => panic!("unexpected unavailable source: {message}"),
    }
}

fn capture(operation: impl FnOnce(&mut Out<'_>) -> Result<()>) -> String {
    let mut text = String::new();
    operation(&mut Out::new(&mut |part| {
        match part {
            Part::Text { text: value } => text.push_str(&value),
            other => panic!("unexpected output {other:?}"),
        }
        Ok(())
    }))
    .unwrap();
    text
}

#[test]
fn exact_workspace_routes_and_method_labels_need_no_models() {
    let fixture = Fixture::new();
    fixture.wiki("needle wiki", "body", &[]);
    fixture.memory("needle memory");
    fixture.file(b"body", "needle.txt");
    fixture
        .owner
        .with_store(|store, signer, _| {
            let sibling = store.collection(
                "wiki",
                CollectionPolicy::new(
                    AdmissionPolicy::Open,
                    AdmissionPolicy::direct(signer.verifying_key()),
                ),
            )?;
            store.commit(
                sibling,
                signer,
                wiki::revision_fragment(genid().id, "needle wrong root", "body", &[], &[]),
            )?;
            Ok(())
        })
        .unwrap();
    let storage = fixture.pin();
    let workspace = storage.selected_workspace().unwrap().id;
    assert_eq!(storage.collection_routes().unwrap().len(), 3);
    let report = Search::with_storage(storage)
        .query(Options {
            query: "needle",
            limit_per_source: DEFAULT_LIMIT,
        })
        .unwrap();
    assert_eq!(report.workspace_revision, Some(format!("{workspace:x}")));
    for (source, scope) in report.sources.iter().zip([WIKI, MEMORY, FILES]) {
        assert_eq!(
            source.collection,
            Some(hex::encode(fixture.routes[&scope].raw))
        );
        assert_eq!(answered(source).1, 1);
    }
    assert_eq!(
        report.sources[0].method,
        "ascii_substring_current_title_and_body"
    );
    assert_eq!(
        report.sources[1].method,
        "bm25_least_summary_handle_per_chunk"
    );
    assert_eq!(
        report.sources[2].method,
        "substring_name_media_type_and_tags_only"
    );
}

#[test]
fn missing_role_is_unavailable_while_other_sources_answer() {
    let fixture = Fixture::new();
    fixture.memory("needle memory");
    fixture.file(b"needle only in file body", "unrelated.txt");
    let routes = BTreeMap::from([
        (MEMORY, fixture.routes[&MEMORY]),
        (FILES, fixture.routes[&FILES]),
    ]);
    let report = Search::with_storage(fixture.owner.clone().with_collections(routes))
        .query(Options {
            query: "needle",
            limit_per_source: 10,
        })
        .unwrap();
    assert!(matches!(&report.sources[0].outcome,
        Outcome::Unavailable { message } if message.contains("unconfigured")));
    assert_eq!(report.sources[0].collection, None);
    assert_eq!(answered(&report.sources[1]).1, 1);
    assert_eq!(
        answered(&report.sources[2]).1,
        0,
        "Files bodies are not searched"
    );
}

#[test]
fn unavailable_wiki_payload_does_not_become_an_empty_answer() {
    let fixture = Fixture::new();
    fixture.memory("needle memory");
    fixture.file(b"body", "needle.txt");
    let missing = "needle absent body".to_owned().to_blob().get_handle();
    let mut fragment = Fragment::empty();
    let title = fragment.put::<blobencodings::UTF8String, _>("needle title".to_owned());
    fragment += wiki::revision_fragment_from_handles(genid().id, title, missing, &[], &[]);
    fixture.publish(WIKI, fragment);
    let report = fixture.query("needle", 10);
    assert!(
        matches!(&report.sources[0].outcome, Outcome::Unavailable { message } if !message.is_empty())
    );
    assert_eq!(
        report.sources[0].collection,
        Some(hex::encode(fixture.routes[&WIKI].raw))
    );
    assert_eq!(answered(&report.sources[1]).1, 1);
    assert_eq!(answered(&report.sources[2]).1, 1);
}

#[test]
fn unavailable_file_tag_fails_only_files_and_ordinary_search_reports_it() {
    let fixture = Fixture::new();
    fixture.wiki("needle wiki", "body", &[]);
    fixture.memory("needle memory");
    let holder = fixture.file(b"body", "unrelated.txt");
    let missing: Inline<inlineencodings::Handle<blobencodings::UTF8String>> =
        "needle missing tag".to_owned().to_blob().get_handle();
    fixture.publish(
        FILES,
        entity! { ExclusiveId::force_ref(&holder) @ file::tag: missing },
    );
    let report = fixture.query("needle", 10);
    assert_eq!(answered(&report.sources[0]).1, 1);
    assert_eq!(answered(&report.sources[1]).1, 1);
    let Outcome::Unavailable { message } = &report.sources[2].outcome else {
        panic!(
            "missing tag bytes must make Files unavailable: {:#?}",
            report.sources[2]
        );
    };
    assert!(
        !message.is_empty(),
        "Files must report its native failure: {:#?}",
        report.sources[2]
    );
    let files = crate::files::Files::with_storage(fixture.selected.clone());
    let error = files
        .search("needle")
        .expect_err("ordinary search must also fail for the missing tag");
    // read() tries exact-byte acquisition after the tag reader's MissingBlob.
    // SharedLocal's closed peer can refuse that acquisition, replacing the
    // inner tag-read context. Assert the actual native error, not one layer's
    // assumed wording; neither path may turn this failure into empty results.
    assert_eq!(
        message,
        &format!("{error:#}"),
        "grouped outcome: {:#?}",
        report.sources[2]
    );
    fixture.publish(
        FILES,
        entity! { ExclusiveId::force_ref(&holder) @
            file::tag: "needle missing tag".to_owned(),
        },
    );
    let restored = fixture.query("needle", 10);
    let (hits, count, _) = answered(&restored.sources[2]);
    assert_eq!(
        count, 1,
        "delivering the exact tag bytes restores the metadata match"
    );
    let Hit::Files(hit) = &hits[0] else {
        panic!("Files")
    };
    assert_eq!(hit.holder, format!("{holder:x}"));
    assert!(hit.tags.iter().any(|tag| tag == "needle missing tag"));
    assert!(files
        .search("needle")
        .unwrap()
        .contains("needle missing tag"));
}

#[test]
fn file_readings_keep_duplicate_content_holders_and_multiple_metadata() {
    let fixture = Fixture::new();
    let first = fixture.file(b"shared bytes", "needle-a.txt");
    let second = fixture.file(b"shared bytes", "needle-b.txt");
    fixture.publish(
        FILES,
        entity! { ExclusiveId::force_ref(&first) @
            file::name: "needle-alternate.txt".to_owned(),
            file::media_type*: crate::files::media_type_fragment("image/png").unwrap(),
        },
    );
    let report = fixture.query("needle", 10);
    let hits = answered(&report.sources[2]).0;
    assert_eq!(
        hits.len(),
        5,
        "two names x two media types, plus another holder"
    );
    let mut holders = BTreeSet::new();
    let mut contents = BTreeSet::new();
    for hit in hits {
        let Hit::Files(hit) = hit else {
            panic!("Files result")
        };
        holders.insert(hit.holder.clone());
        contents.insert(hit.content.clone());
        assert_eq!(
            hit.open,
            OpenTarget::Files {
                id: hit.content.clone()
            }
        );
    }
    assert_eq!(
        holders,
        BTreeSet::from([format!("{first:x}"), format!("{second:x}")])
    );
    assert_eq!(contents.len(), 1);
    let files = crate::files::Files::with_storage(fixture.selected.clone());
    assert_eq!(files.search("needle").unwrap().lines().count(), hits.len());
}

#[test]
fn wiki_current_readings_deduplicate_witnesses_and_open_history_exactly() {
    let fixture = Fixture::new();
    let old = fixture.wiki("needle old", "old body", &[]);
    let old_report = fixture.query("needle", 10);
    let Hit::Wiki(old_hit) = &answered(&old_report.sources[0]).0[0] else {
        panic!("Wiki")
    };
    let new = fixture.wiki("needle current", "new body", &[old]);
    fixture.publish(
        WIKI,
        entity! { ExclusiveId::force_ref(&new) @
            wiki::attrs::author: genid().id,
            wiki::attrs::title: "needle alternative".to_owned(),
        },
    );
    let report = fixture.query("needle", 1);
    let (hits, count, truncated) = answered(&report.sources[0]);
    assert_eq!((hits.len(), count, truncated), (1, 2, true));
    let full = fixture.query("needle", 10);
    for hit in answered(&full.sources[0]).0 {
        let Hit::Wiki(hit) = hit else { panic!("Wiki") };
        assert_eq!(hit.revision, format!("{new:x}"));
    }
    let OpenTarget::Wiki { id, exact } = &old_hit.open else {
        panic!("exact Wiki target")
    };
    assert!(*exact);
    let wiki = crate::wiki::Wiki::with_storage(fixture.selected.clone());
    assert_eq!(wiki.export(id, *exact).unwrap().bytes.as_ref(), b"old body");
    assert!(wiki.show(id, false).unwrap().contains("new body"));
    assert_eq!(
        wiki.search("needle", false, false).unwrap().lines().count(),
        2
    );
}

#[test]
fn wiki_context_is_collected_only_when_requested() {
    let fixture = Fixture::new();
    fixture.wiki(
        "needle title",
        "needle first\nother line\n  needle second",
        &[],
    );
    let wiki = crate::wiki::Wiki::with_storage(fixture.selected.clone());
    let without = wiki.search_hits("needle", false, false).unwrap();
    let with = wiki.search_hits("needle", true, false).unwrap();
    assert_eq!(without.hits.len(), with.hits.len());
    assert!(without.hits[0].matching_lines.is_none());
    assert_eq!(
        with.hits[0].matching_lines.as_deref(),
        Some(["needle first".to_owned(), "needle second".to_owned()].as_slice())
    );
    assert_eq!(
        wiki.search("needle", false, false).unwrap().lines().count(),
        1
    );
    let rendered = wiki.search("needle", true, false).unwrap();
    assert!(rendered.contains("    needle first\n"));
    assert!(rendered.contains("    needle second\n"));
    assert!(!rendered.contains("other line"));
    let report = fixture.query("needle", 10);
    assert_eq!(answered(&report.sources[0]).1, 1);
    let Hit::Wiki(hit) = &answered(&report.sources[0]).0[0] else {
        panic!("Wiki")
    };
    assert!(hit.matching_lines.is_none());
    assert!(
        serde_json::to_value(hit)
            .unwrap()
            .get("matching_lines")
            .is_none(),
        "omitted context must not look like a search with no matching lines"
    );
}

#[test]
fn memory_keeps_the_scored_handle_and_alternative_readings() {
    let fixture = Fixture::new();
    let chunk = fixture.memory("needle cobalt");
    fixture.publish(
        MEMORY,
        entity! { ExclusiveId::force_ref(&chunk) @
            ctx::summary: "needle ochre".to_owned(),
        },
    );
    let report = fixture.query("needle", 10);
    let Hit::Memory(hit) = &answered(&report.sources[1]).0[0] else {
        panic!("Memory")
    };
    assert_eq!(hit.summary_handles.len(), 2);
    assert_eq!(hit.scored_summary, hit.summary_handles[0]);
    assert_eq!(hit.chunk, format!("{chunk:x}"));
    let alternate_only = if hit.summary.contains("cobalt") {
        "ochre"
    } else {
        "cobalt"
    };
    assert_eq!(answered(&fixture.query(alternate_only, 10).sources[1]).1, 0);
    let memory = crate::memory::Memory::with_storage(fixture.selected.clone());
    let rendered = capture(|out| memory.search("needle", out));
    assert!(rendered.contains(&hit.summary));
}

#[test]
fn source_limits_follow_deduplication_and_memory_ties_are_stable() {
    let fixture = Fixture::new();
    let mut ids = Vec::new();
    for _ in 0..3 {
        let id = genid();
        fixture.publish(
            MEMORY,
            entity! { &id @
                metadata::tag: crate::schemas::memory::KIND_CHUNK_ID,
                ctx::summary: "needle equal".to_owned(),
            },
        );
        ids.push(format!("{:x}", id.id));
    }
    ids.sort();
    let first = fixture.query("needle", 2);
    let second = fixture.query("needle", 2);
    assert_eq!(first.json().unwrap(), second.json().unwrap());
    let (hits, count, truncated) = answered(&first.sources[1]);
    assert_eq!((hits.len(), count, truncated), (2, 3, true));
    for (hit, id) in hits.iter().zip(ids) {
        let Hit::Memory(hit) = hit else {
            panic!("Memory")
        };
        assert_eq!(hit.chunk, id);
    }
}

#[test]
fn native_mcp_and_cli_share_the_report_and_catalog_registers_search() {
    let fixture = Fixture::new();
    fixture.wiki("needle", "body", &[]);
    fixture.memory("needle memory");
    fixture.file(b"body", "needle.txt");
    let storage = fixture.pin();
    let workspace = format!("{:x}", storage.selected_workspace().unwrap().id);
    let report = Search::with_storage(storage.clone())
        .query(Options {
            query: "needle",
            limit_per_source: DEFAULT_LIMIT,
        })
        .unwrap();
    let adapter = super::mcp::Search::with_storage(storage.clone());
    let mcp = capture(|out| adapter.call("search", br#"{"query":"needle"}"#.to_vec().into(), out));
    let catalog = crate::mcp::catalog::Catalog::with_storage(
        crate::mcp::catalog::Config::new(fixture.directory.path().join("search.pile")),
        storage,
    );
    assert_eq!(
        catalog
            .tools()
            .filter(|(tool, _)| tool.name == "search")
            .count(),
        1
    );
    let schema: serde_json::Value = serde_json::from_str(adapter.tools()[0].input_schema).unwrap();
    assert_eq!(
        schema["properties"]["limit_per_source"]["default"],
        DEFAULT_LIMIT
    );
    // All frontends publish Report::json: compare that actual wire format.
    // to_value promotes an f32 directly to f64, unlike serialized f32 JSON.
    let native_json: serde_json::Value = serde_json::from_str(&report.json().unwrap()).unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&mcp).unwrap(),
        native_json
    );
    fixture.owner.close().unwrap();
    let cli = super::cli::Cli::try_parse_from([
        "search".to_owned(),
        "--pile".to_owned(),
        fixture
            .directory
            .path()
            .join("search.pile")
            .display()
            .to_string(),
        "--key".to_owned(),
        fixture
            .directory
            .path()
            .join("search.key")
            .display()
            .to_string(),
        "--workspace".to_owned(),
        workspace,
        "query".to_owned(),
        "needle".to_owned(),
    ])
    .unwrap();
    let cli = capture(|out| super::cli::execute(cli, out));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&cli).unwrap(),
        native_json
    );
}

#[test]
fn invalid_queries_and_limits_fail_before_source_work() {
    let storage = Storage::new("/unused/search-test.pile".into(), None);
    let search = Search::with_storage(storage.clone());
    for options in [
        Options {
            query: " ",
            limit_per_source: 10,
        },
        Options {
            query: "q",
            limit_per_source: 0,
        },
    ] {
        assert!(search.query(options).is_err());
    }
    let adapter = super::mcp::Search::with_storage(storage);
    for args in [
        r#"{"query":" "}"#,
        r#"{"query":"q","limit_per_source":0}"#,
        r#"{"query":"q","unknown":true}"#,
    ] {
        let error = adapter
            .call(
                "search",
                args.as_bytes().to_vec().into(),
                &mut Out::new(&mut |_| panic!("invalid arguments cannot emit results")),
            )
            .unwrap_err();
        assert!(!error.to_string().is_empty());
    }
}
