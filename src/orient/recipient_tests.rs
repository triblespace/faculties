//! Recipient-scoped Presented receipts in the shared Spawn collection.
//!
//! These tests run the real CLI operations (`poll`, `wait`, `show`) against
//! explicit workspace routes that name no `orient-receipts` role, on two piles
//! with two node keys that replicate one shared root. A receipt counts for the
//! zooid it names, never for the node that signed it.

use super::*;
use crate::schemas::spawn::DEFAULT_SCOPE_ID as SPAWN_SCOPE_ID;
use triblespace::core::capability::policy::AdmissionPolicy as Admission;
use triblespace::core::collection::CollectionPolicy;
use triblespace::core::repo::StorageClose as _;

/// One node: its own pile file and its own durable signing key.
struct Node {
    path: PathBuf,
    key: PathBuf,
    signer: SigningKey,
}

impl Node {
    fn new(dir: &Path, name: &str) -> Self {
        let path = dir.join(format!("{name}.pile"));
        let key = dir.join(format!("{name}.key"));
        std::fs::File::create(&path).unwrap();
        let signer = crate::storage::initialize_signer(&path, Some(&key)).unwrap();
        Self { path, key, signer }
    }

    fn open(&self) -> FacultyStore {
        open_store_as(&self.path, self.signer.verifying_key()).unwrap()
    }
}

/// Every faculty role, routed to one shared descriptor whose READ and WRITE
/// both admit either node key (threshold 1), as the colony's roots do. The
/// routes deliberately contain no `orient-receipts` role.
fn shared_routes(pile: &mut FacultyStore, nodes: &[&Node]) -> crate::storage::CollectionRoutes {
    let roots: Vec<_> = nodes
        .iter()
        .map(|node| node.signer.verifying_key())
        .collect();
    let quorum = || Admission::quorum(roots.iter().copied(), 1, None).unwrap();
    let mut routes = crate::storage::CollectionRoutes::new();
    for (scope, name) in crate::collection_names::table() {
        if scope == crate::schemas::orient::RECEIPTS_SCOPE_ID {
            continue;
        }
        let collection = pile
            .collection(name, CollectionPolicy::new(quorum(), quorum()))
            .unwrap();
        routes.insert(scope, collection.handle());
    }
    routes
}

/// Independent replication: every blob and record `from` holds, appended to
/// `to`. Nothing is shared between the two files but these bytes.
fn replicate(from: &Node, to: &Node) {
    let mut source = from.open();
    let snapshot = source.snapshot().unwrap();
    let mut target = to.open();
    for info in snapshot.blobs() {
        let info = info.unwrap();
        let blob: Blob<blobencodings::UnknownBlob> =
            BlobStoreGet::get(&snapshot, info.handle).unwrap();
        target.put::<blobencodings::UnknownBlob, _>(blob).unwrap();
    }
    for record in snapshot.records().unwrap() {
        target.insert(record.unwrap()).unwrap();
    }
    target.close().unwrap();
    source.close().unwrap();
}

fn storage(node: &Node, routes: &crate::storage::CollectionRoutes) -> Storage {
    Storage::new(node.path.clone(), Some(node.key.clone())).with_collections(routes.clone())
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

fn collect(text: &mut String) -> impl FnMut(crate::out::Part) -> Result<()> + '_ {
    move |part| {
        let crate::out::Part::Text { text: part } = part else {
            bail!("expected text")
        };
        text.push_str(&part);
        Ok(())
    }
}

/// A cold `orient poll` on a freshly opened store: nothing carried over.
fn poll(
    node: &Node,
    routes: &crate::storage::CollectionRoutes,
    persona: &str,
    peek: bool,
) -> String {
    let routing = storage(node, routes);
    let mut pile = node.open();
    let mut text = String::new();
    runtime()
        .block_on(cmd_poll(
            &routing,
            &mut pile,
            &node.signer,
            Some(persona),
            peek,
            Duration::from_secs(180),
            None,
            &mut Out::new(&mut collect(&mut text)),
        ))
        .unwrap();
    pile.close().unwrap();
    text
}

/// A cold one-shot `orient wait`, which also presents due habit occurrences.
fn wait(
    node: &Node,
    routes: &crate::storage::CollectionRoutes,
    persona: &str,
    timeout: Duration,
) -> String {
    let routing = storage(node, routes);
    let mut pile = node.open();
    let mut text = String::new();
    let options = WaitOptions {
        timeout: Some(timeout),
        poll_interval: Duration::from_millis(50),
        form: NewsForm::Report,
    };
    runtime()
        .block_on(async {
            tokio::time::timeout(
                timeout + Duration::from_secs(30),
                cmd_wait(
                    &routing,
                    &mut pile,
                    &node.signer,
                    &node.path,
                    Some(persona),
                    &options,
                    Duration::from_secs(180),
                    None,
                    &mut Out::new(&mut collect(&mut text)),
                ),
            )
            .await
        })
        .expect("a bounded wait ends")
        .unwrap();
    pile.close().unwrap();
    text
}

/// A one-shot wait that reported nothing and ended at its timeout.
fn timed_out_quietly(text: &str) -> bool {
    text.trim() == "No change detected since wait started."
}

/// Receipt facts in the routed Spawn collection, read from committed bytes.
fn spawn_facts(node: &Node, routes: &crate::storage::CollectionRoutes) -> TribleSet {
    let mut pile = node.open();
    let snapshot = pile.snapshot().unwrap();
    let collection = triblespace::core::collection::Collection::<SimpleArchive>::open(
        &snapshot,
        routes[&SPAWN_SCOPE_ID],
    )
    .unwrap();
    let facts = snapshot
        .collection(collection)
        .unwrap()
        .view::<TribleSet>()
        .unwrap();
    pile.close().unwrap();
    facts
}

fn person(
    pile: &mut FacultyStore,
    signer: &SigningKey,
    relations: CollectionHandle,
    label: &str,
) -> Id {
    let person = *fucid();
    let (fragment, _, _) = relations::person_fragment(
        person,
        relations::ProfileInput {
            label: label.to_owned(),
            ..Default::default()
        },
    )
    .unwrap();
    let collection = triblespace::core::collection::Collection::<SimpleArchive>::open(
        &pile.snapshot().unwrap(),
        relations,
    )
    .unwrap();
    pile.commit(collection, signer, fragment).unwrap();
    person
}

fn commit_to(
    pile: &mut FacultyStore,
    signer: &SigningKey,
    handle: CollectionHandle,
    fragment: Fragment,
) {
    let collection = triblespace::core::collection::Collection::<SimpleArchive>::open(
        &pile.snapshot().unwrap(),
        handle,
    )
    .unwrap();
    pile.commit(collection, signer, fragment).unwrap();
}

/// The acceptance scenario. One goal assigned to both A and B, and one habit
/// addressed to everyone, are offered to both through one shared root.
/// Delivering to A on node X suppresses them for A only. On node Y, which holds
/// an independent replica and its own key, a cold A is quiet and a cold B is
/// still offered both; delivering to B there writes B's own receipts, and the
/// merged replicas hold both zooids' seen state.
#[test]
fn a_delivery_is_seen_only_by_its_recipient_across_replicas() {
    let dir = tempfile::tempdir().unwrap();
    let x = Node::new(dir.path(), "x");
    let y = Node::new(dir.path(), "y");

    let routes = {
        let mut pile = x.open();
        let routes = shared_routes(&mut pile, &[&x, &y]);
        let alice = person(
            &mut pile,
            &x.signer,
            routes[&RELATIONS_SCOPE_ID],
            "alice-zooid",
        );
        let bob = person(
            &mut pile,
            &x.signer,
            routes[&RELATIONS_SCOPE_ID],
            "bob-zooid",
        );
        assert_ne!(alice, bob);
        let (goal, _) = compass::goal_fragment(
            "shared recipient goal",
            vec!["alice-zooid".to_owned(), "bob-zooid".to_owned()],
            None,
            clock::point_now().unwrap(),
        )
        .unwrap();
        commit_to(&mut pile, &x.signer, routes[&COMPASS_SCOPE_ID], goal);
        let (habit, _) = habits::habit_fragment(
            "shared-recipient-check",
            "every 1m",
            "look once",
            None,
            &[],
            &[],
        )
        .unwrap();
        commit_to(&mut pile, &x.signer, routes[&HABIT_SCOPE_ID], habit);
        pile.close().unwrap();
        routes
    };

    // Offered to both, on X; peeking records nothing.
    for persona in ["alice-zooid", "bob-zooid"] {
        let peek = poll(&x, &routes, persona, true);
        assert!(peek.contains("shared recipient goal"), "{persona}: {peek}");
    }
    assert!(!exists!(pattern!(&spawn_facts(&x, &routes), [{
        _?receipt @ metadata::tag: &KIND_PRESENTED
    }])));

    // Deliver to A only, on X.
    let delivered = wait(&x, &routes, "alice-zooid", Duration::from_secs(10));
    assert!(delivered.contains("shared recipient goal"), "{delivered}");
    assert!(delivered.contains("shared-recipient-check"), "{delivered}");

    // Every receipt X wrote names A.
    let facts = spawn_facts(&x, &routes);
    let named: BTreeSet<Id> = find!(
        persona: Id,
        pattern!(&facts, [{ _?receipt @
            metadata::tag: &KIND_PRESENTED,
            presentation::persona: ?persona,
        }])
    )
    .collect();
    assert_eq!(named.len(), 1, "every receipt X wrote names one recipient");

    // Restart on X: A is quiet, B still offered.
    assert!(poll(&x, &routes, "alice-zooid", true).is_empty());
    assert!(poll(&x, &routes, "bob-zooid", true).contains("shared recipient goal"));

    // Y holds only what replication carried; it has its own key.
    replicate(&x, &y);
    assert_ne!(x.signer.verifying_key(), y.signer.verifying_key());
    assert!(
        poll(&y, &routes, "alice-zooid", true).is_empty(),
        "A's receipt travels with the shared root"
    );
    let quiet = wait(&y, &routes, "alice-zooid", Duration::from_secs(2));
    assert!(
        timed_out_quietly(&quiet),
        "A's habit occurrence is seen too: {quiet}"
    );
    let offered = poll(&y, &routes, "bob-zooid", true);
    assert!(offered.contains("shared recipient goal"), "{offered}");

    // Deliver to B separately, on Y, with Y's key.
    let delivered = wait(&y, &routes, "bob-zooid", Duration::from_secs(10));
    assert!(delivered.contains("shared recipient goal"), "{delivered}");
    assert!(delivered.contains("shared-recipient-check"), "{delivered}");
    assert!(poll(&y, &routes, "bob-zooid", true).is_empty());
    let quiet = wait(&y, &routes, "bob-zooid", Duration::from_secs(2));
    assert!(timed_out_quietly(&quiet), "{quiet}");

    // Back on X after replication: both quiet, each by its own receipts.
    replicate(&y, &x);
    assert!(poll(&x, &routes, "alice-zooid", true).is_empty());
    assert!(poll(&x, &routes, "bob-zooid", true).is_empty());
    let facts = spawn_facts(&x, &routes);
    let named: BTreeSet<Id> = find!(
        persona: Id,
        pattern!(&facts, [{ _?receipt @
            metadata::tag: &KIND_PRESENTED,
            presentation::persona: ?persona,
        }])
    )
    .collect();
    assert_eq!(named.len(), 2, "A's and B's receipts, side by side");
    let mut pile = x.open();
    let snapshot = pile.snapshot().unwrap();
    let signers: BTreeSet<[u8; 32]> = snapshot
        .records()
        .unwrap()
        .map(Result::unwrap)
        .filter(|record| record.collection() == routes[&SPAWN_SCOPE_ID])
        .filter_map(|record| match record {
            triblespace::core::collection::CollectionRecord::Commit(commit) => {
                Some(commit.public_key().raw)
            }
            _ => None,
        })
        .collect();
    pile.close().unwrap();
    assert_eq!(signers.len(), 2, "each node signed its own receipts");
}

/// A delivery that fails before acceptance records nothing, for anyone; the
/// accepted retry records exactly the recipient the report was prepared for.
#[test]
fn a_failed_delivery_presents_nothing_and_an_accepted_one_names_its_recipient() {
    let dir = tempfile::tempdir().unwrap();
    let x = Node::new(dir.path(), "x");
    let mut pile = x.open();
    let routes = shared_routes(&mut pile, &[&x]);
    let routing = storage(&x, &routes);
    let rt = runtime();
    let receipts = rt
        .block_on(ReceiptSource::open(&routing, &mut pile, &x.signer))
        .unwrap();
    assert_eq!(receipts.source.handle(), routes[&SPAWN_SCOPE_ID]);
    let alice = *fucid();
    let bob = *fucid();
    let event = *fucid();
    let news = News::Report {
        observer: alice,
        text: "News: for alice\n".to_owned(),
        stream: "news: for alice\n".to_owned(),
        events: vec![event],
    };
    let error = rt
        .block_on(apply_prepared_news(
            &mut pile,
            &x.signer,
            receipts.source,
            false,
            &news,
            "",
            NewsForm::Report,
            &mut Out::new(&mut |_| bail!("callback rejected the report")),
            &mut |_, _, _| {},
        ))
        .unwrap_err();
    assert!(format!("{error:#}").contains("callback rejected"));
    let facts = pile
        .snapshot()
        .unwrap()
        .collection(receipts.source)
        .unwrap()
        .view::<TribleSet>()
        .unwrap();
    assert!(crate::orient::presented_events(&facts, alice).is_empty());
    assert!(crate::orient::presented_events(&facts, bob).is_empty());

    rt.block_on(apply_prepared_news(
        &mut pile,
        &x.signer,
        receipts.source,
        false,
        &news,
        "",
        NewsForm::Report,
        &mut Out::new(&mut |_| Ok(())),
        &mut |_, _, _| {},
    ))
    .unwrap();
    let facts = pile
        .snapshot()
        .unwrap()
        .collection(receipts.source)
        .unwrap()
        .view::<TribleSet>()
        .unwrap();
    assert_eq!(
        crate::orient::presented_events(&facts, alice),
        BTreeSet::from([event])
    );
    assert!(crate::orient::presented_events(&facts, bob).is_empty());
    pile.close().unwrap();
}

/// Receipts written before 2026-11-23 name no persona: the signing key stood
/// in for the observer, and many zooids shared each key. Their owner is not
/// provable, so they suppress nobody -- not A, not B -- while staying readable
/// as the facts they are. A legacy mixed-ledger receipt does name its persona,
/// and its explicit import keeps that persona and stays idempotent.
#[test]
fn persona_less_receipts_suppress_nobody_and_attributable_imports_keep_their_persona() {
    let dir = tempfile::tempdir().unwrap();
    let x = Node::new(dir.path(), "x");
    let (routes, event) = {
        let mut pile = x.open();
        let routes = shared_routes(&mut pile, &[&x]);
        person(
            &mut pile,
            &x.signer,
            routes[&RELATIONS_SCOPE_ID],
            "alice-zooid",
        );
        person(
            &mut pile,
            &x.signer,
            routes[&RELATIONS_SCOPE_ID],
            "bob-zooid",
        );
        let (goal, goal_id) = compass::goal_fragment(
            "legacy receipt goal",
            vec!["alice-zooid".to_owned(), "bob-zooid".to_owned()],
            None,
            clock::point_now().unwrap(),
        )
        .unwrap();
        commit_to(&mut pile, &x.signer, routes[&COMPASS_SCOPE_ID], goal);
        // The pre-change shape, exactly as the old writer built it, in the
        // receipt root itself.
        let old = entity! {
            metadata::tag: &KIND_PRESENTED,
            presentation::event: &goal_id,
        } + entity! { _ @
            metadata::tag: &KIND_PRESENTED,
            presentation::habit: &goal_id,
            presentation::due_at: clock::point(Epoch::from_tai_seconds(0.0)).unwrap(),
        };
        commit_to(&mut pile, &x.signer, routes[&SPAWN_SCOPE_ID], old);
        pile.close().unwrap();
        (routes, goal_id)
    };
    for persona in ["alice-zooid", "bob-zooid"] {
        let offered = poll(&x, &routes, persona, true);
        assert!(
            offered.contains("legacy receipt goal"),
            "{persona}: {offered}"
        );
    }
    let facts = spawn_facts(&x, &routes);
    assert!(
        exists!(pattern!(&facts, [{ _?receipt @
            metadata::tag: &KIND_PRESENTED,
            presentation::event: &event,
        }])),
        "the old receipt is still there, readable, unrewritten"
    );

    // An attributable legacy receipt: the mixed ledger names its persona.
    let alice = {
        let mut pile = x.open();
        let snapshot = pile.snapshot().unwrap();
        let relations = triblespace::core::collection::Collection::<SimpleArchive>::open(
            &snapshot,
            routes[&RELATIONS_SCOPE_ID],
        )
        .unwrap();
        let facts = snapshot
            .collection(relations)
            .unwrap()
            .view::<TribleSet>()
            .unwrap();
        let archive = FactArchive::new(vec![
            triblespace::core::blob::encodings::succinctarchive::SuccinctArchive::from(&facts),
        ]);
        let alice = resolve_resident_persona(&archive, &snapshot, "alice-zooid").unwrap();
        commit_to(
            &mut pile,
            &x.signer,
            routes[&crate::schemas::orient::DEFAULT_SCOPE_ID],
            orient_model::presented_fragment(alice, [event]),
        );
        pile.close().unwrap();
        alice
    };
    let routing = storage(&x, &routes);
    let orient = Orient::with_storage(routing.clone());
    assert_eq!(orient.import_receipts("alice-zooid").unwrap(), 1);
    let once = spawn_facts(&x, &routes);
    assert_eq!(orient.import_receipts("alice-zooid").unwrap(), 1);
    assert_eq!(
        spawn_facts(&x, &routes),
        once,
        "a replayed import adds nothing"
    );
    routing.close().unwrap();
    assert_eq!(
        crate::orient::presented_events(&once, alice),
        BTreeSet::from([event])
    );
    assert!(poll(&x, &routes, "alice-zooid", true).is_empty());
    assert!(poll(&x, &routes, "bob-zooid", true).contains("legacy receipt goal"));
}

/// The Spawn collection carries Spawn's own records -- including a run that
/// names a persona with Spawn's attribute -- beside receipts. Neither reads as
/// the other: a run's persona is never a receipt, an untagged entity with the
/// presentation attributes is not one either, and writing receipts leaves
/// Spawn's own view of its records unchanged.
#[test]
fn spawn_records_and_receipts_share_one_root_without_being_mistaken() {
    let dir = tempfile::tempdir().unwrap();
    let x = Node::new(dir.path(), "x");
    let (routes, alice, goal_id) = {
        let mut pile = x.open();
        let routes = shared_routes(&mut pile, &[&x]);
        let alice = person(
            &mut pile,
            &x.signer,
            routes[&RELATIONS_SCOPE_ID],
            "alice-zooid",
        );
        let (goal, goal_id) = compass::goal_fragment(
            "mixed collection goal",
            vec!["alice-zooid".to_owned()],
            None,
            clock::point_now().unwrap(),
        )
        .unwrap();
        commit_to(&mut pile, &x.signer, routes[&COMPASS_SCOPE_ID], goal);
        let (mut spawn, spawn_id, _, _) =
            crate::spawn::spawn_fragment("a mixed-collection brief").unwrap();
        let (run, _) = crate::spawn::run_fragment(&crate::spawn::RunFacts {
            spawn: spawn_id,
            started_at: clock::point_now().unwrap(),
            persona: alice,
            spawner: alice,
            unit: "liora-mixed.service".to_owned(),
            workdir: "/tmp".to_owned(),
            effort: "low".to_owned(),
            prompt: "prompt".to_owned(),
            provider: "claude".to_owned(),
            provider_reason: "fixture".to_owned(),
            model: None,
            model_unset: Some("fixture".to_owned()),
            untinted: None,
            request: None,
            resumes: None,
            cover: None,
        });
        spawn += run;
        // Presentation attributes without the receipt tag are not a receipt.
        spawn += entity! {
            presentation::persona: &alice,
            presentation::event: &goal_id,
        };
        commit_to(&mut pile, &x.signer, routes[&SPAWN_SCOPE_ID], spawn);
        pile.close().unwrap();
        (routes, alice, goal_id)
    };
    let zooids = |routes: &crate::storage::CollectionRoutes| {
        let routing = storage(&x, routes);
        let views = crate::spawn::Spawn::new(
            routing.clone(),
            crate::spawn::LaunchConfig::under_home(dir.path()),
        )
        .list()
        .unwrap();
        routing.close().unwrap();
        format!("{views:?}")
    };
    let before = zooids(&routes);
    assert!(before.contains("a mixed-collection brief"), "{before}");

    let offered = poll(&x, &routes, "alice-zooid", true);
    assert!(offered.contains("mixed collection goal"), "{offered}");
    let delivered = poll(&x, &routes, "alice-zooid", false);
    assert!(delivered.contains("mixed collection goal"), "{delivered}");
    assert!(poll(&x, &routes, "alice-zooid", true).is_empty());
    let facts = spawn_facts(&x, &routes);
    assert_eq!(
        crate::orient::presented_events(&facts, alice),
        BTreeSet::from([goal_id])
    );
    assert_eq!(
        zooids(&routes),
        before,
        "receipts do not change Spawn's records"
    );
}

/// With no routes and no environment override, Orient still opens this
/// signer's private Spawn root, and receipts there are scoped the same way.
#[test]
fn the_unconfigured_cli_default_opens_the_private_spawn_root() {
    let dir = tempfile::tempdir().unwrap();
    let x = Node::new(dir.path(), "x");
    let mut pile = x.open();
    let rt = runtime();
    let opened = rt
        .block_on(ReceiptSource::open(&test_storage(), &mut pile, &x.signer))
        .unwrap();
    let private = ReceiptSource::register(&mut pile, &x.signer).unwrap();
    assert_eq!(opened.source, private.source);
    let expected =
        crate::collection_names::open(&mut pile, SPAWN_SCOPE_ID, x.signer.verifying_key()).unwrap();
    assert_eq!(opened.source, expected);
    let alice = *fucid();
    let bob = *fucid();
    save_presentations(&mut pile, &x.signer, opened.source, alice, [goal_like()]).unwrap();
    let facts = pile
        .snapshot()
        .unwrap()
        .collection(opened.source)
        .unwrap()
        .view::<TribleSet>()
        .unwrap();
    assert_eq!(
        crate::orient::presented_events(&facts, alice),
        BTreeSet::from([goal_like()])
    );
    assert!(crate::orient::presented_events(&facts, bob).is_empty());
    pile.close().unwrap();
}

fn goal_like() -> Id {
    Id::new([0x42; 16]).unwrap()
}
