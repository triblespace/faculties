//! `knowledge transfer` between scratch workspaces in one scratch pile.
//!
//! The source Knowledge admits a second key as a writer. That key authors and
//! signs every source revision, so the original author and signer differ from
//! the importing node, which only curates in the destination.

use super::*;
use crate::files::operations::publish_fault;
use crate::schemas::{config::config, workspace as workspace_attrs};
use crate::storage::initialize_signer;
use anybytes::Bytes;
use ed25519_dalek::SigningKey;
use hifitime::Epoch;
use std::fs::File;
use triblespace::core::collection::{AdmissionPolicy, CollectionHandle, CollectionPolicy};

const PRIVATE_NOTE: &str = "the old VPN password is in the blue binder";
const ATTACHMENT: &[u8] = b"\x89PNG synthetic diagram";
const UNRELATED: &[u8] = b"personnel list, not for the team";

struct Fixture {
    _directory: tempfile::TempDir,
    pile: PathBuf,
    key: PathBuf,
    node: SigningKey,
    original: SigningKey,
    /// Source: Knowledge + Files. Destination: Knowledge + Files.
    source: Id,
    destination: Id,
    /// Binds the source's Knowledge and Files roots again.
    same_root: Id,
    /// A Knowledge-only destination.
    knowledge_only: Id,
    source_knowledge: CollectionHandle,
    source_files: CollectionHandle,
    destination_files: CollectionHandle,
    private_parent: Id,
    guide: Id,
    linked: Id,
    attachment: Id,
    unrelated: Id,
    bundle: Id,
}

fn at(seconds: f64) -> Inline<inlineencodings::NsTAIInterval> {
    let epoch = Epoch::from_tai_seconds(seconds);
    (epoch, epoch).try_to_inline().unwrap()
}

fn content_hex(bytes: &[u8]) -> String {
    let staged = crate::files::stage(Bytes::from(bytes.to_vec()), "x", "text/plain").unwrap();
    let content = find!(
        content: ContentHandle,
        pattern!(staged.facts(), [{ _?file @ file::content: ?content }])
    )
    .next()
    .unwrap();
    content_hash_hex(content)
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let pile = directory.path().join("node.pile");
        let key = directory.path().join("node.key");
        File::create(&pile).unwrap();
        let node = initialize_signer(&pile, Some(&key)).unwrap();
        let original = SigningKey::from_bytes(&[91; 32]);
        let third = SigningKey::from_bytes(&[92; 32]);
        let storage = Storage::new(pile.clone(), Some(key.clone()));
        let n = node.verifying_key();
        let both = AdmissionPolicy::quorum([n, original.verifying_key()], 1, None).unwrap();
        let with_third = AdmissionPolicy::quorum([n, third.verifying_key()], 1, None).unwrap();

        let (source_knowledge, destination_knowledge, knowledge_only_root, source_files, destination_files) =
            storage
                .with_store(|store, _, _| {
                    let mut root =
                        |name: &str, policy: CollectionPolicy| -> Result<CollectionHandle> {
                            Ok(store.collection(name, policy)?.handle())
                        };
                    Ok((
                        root(
                            "wiki",
                            CollectionPolicy::new(AdmissionPolicy::direct(n), both.clone()),
                        )?,
                        root("wiki", crate::collection_names::private_policy(n))?,
                        root(
                            "wiki",
                            CollectionPolicy::new(
                                AdmissionPolicy::Open,
                                AdmissionPolicy::direct(n),
                            ),
                        )?,
                        root("files", crate::collection_names::private_policy(n))?,
                        root(
                            "files",
                            CollectionPolicy::new(AdmissionPolicy::direct(n), with_third.clone()),
                        )?,
                    ))
                })
                .unwrap();

        // Source content, authored and signed by the original key.
        let (author_facts, author) = knowledge_model::author_record(&original.verifying_key());
        let revision = |title: &str, content: &str, predecessors: &[Id], time: f64| {
            knowledge_model::revision_record(RevisionDraft {
                title: title.into(),
                content: content.into(),
                tags: BTreeSet::new(),
                predecessors: predecessors.iter().copied().collect(),
                author,
                authored_at: at(time),
            })
            .unwrap()
        };
        let (private_facts, private_parent) = revision("IT notes", PRIVATE_NOTE, &[], 1_000.0);
        let (linked_facts, linked) =
            revision("Printer setup", "Use the hall printer.", &[], 1_100.0);
        let guide_text = format!(
            "= Onboarding guide\nSee #link(\"wiki:{linked:x}\")[printers] and #link(\"files:{}\")[the diagram].",
            content_hex(ATTACHMENT)
        );
        let (guide_facts, guide) =
            revision("Onboarding guide", &guide_text, &[private_parent], 1_200.0);

        let stage = |bytes: &[u8], name: &str| {
            let staged =
                crate::files::stage(Bytes::from(bytes.to_vec()), name, "image/png").unwrap();
            let id = staged.root().unwrap();
            (staged, id)
        };
        let (attachment_facts, attachment) = stage(ATTACHMENT, "diagram.png");
        let (unrelated_facts, unrelated) = stage(UNRELATED, "people.png");
        let (bundle_facts, bundle) = stage(b"git bundle", "history.bundle");
        let provenance = schema::source_provenance_fragment(
            guide,
            "c0ffee",
            "Onboarding.md",
            "Old Admin <admin@example.invalid>",
            Some(at(900.0)),
            Some(bundle),
        );

        storage
            .with_store(|store, _, _| {
                let snapshot = store.snapshot()?;
                let knowledge = Collection::open(&snapshot, source_knowledge)?;
                let mut fragment = author_facts.clone();
                fragment += private_facts;
                fragment += linked_facts;
                fragment += guide_facts;
                fragment += provenance;
                store.commit(knowledge, &original, fragment)?;
                let files = Collection::open(&snapshot, source_files)?;
                let mut fragment = attachment_facts;
                fragment += unrelated_facts;
                fragment += bundle_facts;
                store.commit(files, &node, fragment)?;
                Ok(())
            })
            .unwrap();

        let fixture_routes = |knowledge: CollectionHandle, files: Option<CollectionHandle>| {
            let mut routes = entity! {
                config::anchor: schema::DEFAULT_SCOPE_ID,
                config::selects: knowledge,
            };
            if let Some(files) = files {
                routes += entity! { config::anchor: FILES_SCOPE_ID, config::selects: files };
            }
            routes
        };
        let mut bindings = Fragment::empty();
        let mut bind = |label: &str, knowledge, files| {
            let revision = genid();
            let id = revision.id;
            bindings += entity! { &revision @
                metadata::tag: workspace_attrs::WORKSPACE,
                config::anchor: genid().id,
                workspace_attrs::enrolled: true,
                metadata::name: label,
                workspace_attrs::workspace_route*: fixture_routes(knowledge, files),
            };
            id
        };
        let source = bind("IT", source_knowledge, Some(source_files));
        let destination = bind("Team", destination_knowledge, Some(destination_files));
        let same_root = bind("IT again", source_knowledge, Some(source_files));
        let knowledge_only = bind("Notes", knowledge_only_root, None);
        storage
            .with_store(|store, signer, _| {
                let config = store.collection(
                    crate::config::COLLECTION_NAME,
                    crate::collection_names::private_policy(signer.verifying_key()),
                )?;
                store.commit(config, signer, bindings)?;
                Ok(())
            })
            .unwrap();

        Self {
            _directory: directory,
            pile,
            key,
            node,
            original,
            source,
            destination,
            same_root,
            knowledge_only,
            source_knowledge,
            source_files,
            destination_files,
            private_parent,
            guide,
            linked,
            attachment,
            unrelated,
            bundle,
        }
    }

    fn storage(&self, workspace: Id) -> Result<Storage> {
        crate::cli_workspace::WorkspaceArgs {
            workspace: Some(workspace),
        }
        .storage(self.pile.clone(), Some(self.key.clone()))
    }

    /// A later source commit by the original key.
    fn commit_source(&self, fragment: Fragment) {
        Storage::new(self.pile.clone(), Some(self.key.clone()))
            .with_store(|store, _, _| {
                let knowledge = Collection::open(&store.snapshot()?, self.source_knowledge)?;
                store.commit(knowledge, &self.original, fragment)?;
                Ok(())
            })
            .unwrap();
    }

    fn knowledge(&self, workspace: Id) -> Knowledge {
        Knowledge::with_storage(self.storage(workspace).unwrap())
    }

    fn transfer(&self, from: Id, to: Id, options: TransferOptions) -> Result<TransferReport> {
        self.knowledge(from)
            .transfer(&self.knowledge(to), self.guide, options)
    }

    /// Every Knowledge fact the workspace's collection holds, read whole.
    fn knowledge_facts(&self, workspace: Id) -> FactArchive {
        KnowledgeStorage {
            storage: &self.storage(workspace).unwrap(),
        }
        .view(|view| Ok(view.facts.clone()))
        .unwrap()
    }

    fn files_ids(&self, workspace: Id) -> BTreeSet<Id> {
        KnowledgeStorage {
            storage: &self.storage(workspace).unwrap(),
        }
        .view_with_scope(FILES_SCOPE_ID, "Files", |_, files| {
            Ok(find!(id: Id, pattern!(files, [{ ?id @ metadata::tag: &KIND_FILE }])).collect())
        })
        .unwrap()
    }
}

/// Revisions, authorship and provenance records a Knowledge holds.
fn census(facts: &FactArchive) -> (usize, usize, usize) {
    let tagged =
        |kind: &Id| find!(id: Id, pattern!(facts, [{ ?id @ metadata::tag: kind }])).count();
    (
        knowledge_model::revision_ids(facts).len(),
        tagged(&schema::KIND_AUTHORSHIP),
        tagged(&schema::KIND_SOURCE_PROVENANCE),
    )
}

fn reference<'a>(report: &'a TransferReport, kind: ReferenceKind) -> Vec<&'a Reference> {
    report
        .references
        .iter()
        .filter(|reference| reference.kind == kind)
        .collect()
}

fn carried(report: &TransferReport) -> BTreeSet<(Id, Reason)> {
    report
        .carried
        .iter()
        .map(|item| (item.id, item.reason))
        .collect()
}

#[test]
fn dry_run_names_the_exact_scope_and_publishes_nothing() {
    let fixture = Fixture::new();
    let report = fixture
        .transfer(
            fixture.source,
            fixture.destination,
            TransferOptions {
                dry_run: true,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        carried(&report),
        BTreeSet::from([(fixture.guide, Reason::Requested)])
    );
    assert_eq!(report.publications, 0);
    assert!(report.new_knowledge_facts > 0);
    assert!(!report.verified);
    let parents = reference(&report, ReferenceKind::Parent);
    assert_eq!(parents.len(), 1);
    assert_eq!(parents[0].target, format!("{:x}", fixture.private_parent));
    assert_eq!(parents[0].at_destination, Presence::Absent);
    let links = reference(&report, ReferenceKind::Link);
    assert_eq!(links[0].target, format!("{:x}", fixture.linked));
    let files = reference(&report, ReferenceKind::File);
    assert_eq!(files[0].target, content_hex(ATTACHMENT));
    assert_eq!(files[0].at_destination, Presence::Absent);
    let bundles = reference(&report, ReferenceKind::ProvenanceBundle);
    assert_eq!(bundles[0].target, format!("{:x}", fixture.bundle));
    assert!(fixture
        .knowledge(fixture.destination)
        .show(&format!("{:x}", fixture.guide), true)
        .is_err());
}

#[test]
fn a_transferred_revision_keeps_its_identity_author_and_time_without_its_private_parent() {
    let fixture = Fixture::new();
    let source_before = census(&fixture.knowledge_facts(fixture.source));
    let report = fixture
        .transfer(
            fixture.source,
            fixture.destination,
            TransferOptions::default(),
        )
        .unwrap();
    assert!(report.verified);
    assert_eq!(report.publications, 1, "Knowledge only: no file was opted in");
    assert_eq!(report.new_file_facts, 0);

    let source = fixture.knowledge_facts(fixture.source);
    let destination = fixture.knowledge_facts(fixture.destination);
    assert_eq!(
        census(&source),
        source_before,
        "the source is never written"
    );
    let original_author = knowledge_model::author_record(&fixture.original.verifying_key()).1;
    let node_author = knowledge_model::author_record(&fixture.node.verifying_key()).1;
    assert_ne!(original_author, node_author);
    let at_source = knowledge_model::revision_records(&source, fixture.guide);
    let at_destination = knowledge_model::revision_records(&destination, fixture.guide);
    assert_eq!(at_source, at_destination, "same facts, authorship and time");
    assert_eq!(at_destination[0].author, Some(original_author));
    assert_eq!(
        at_destination[0].authored_at(),
        Some(at(1_200.0)),
        "the original authorship time, not the transfer time"
    );
    assert!(
        knowledge_model::revision_records(&destination, fixture.private_parent).is_empty(),
        "the excluded parent crosses as an id only"
    );
    let private_handle: schema::TextHandle = Fragment::empty().put(PRIVATE_NOTE.to_owned());
    assert!(!exists!(
        (),
        pattern!(&destination, [{ _?entity @ schema::attrs::content: &private_handle }])
    ));
    assert!(fixture.files_ids(fixture.destination).is_empty());

    // The identical citation resolves to identical bytes in both workspaces.
    let id = format!("{:x}", fixture.guide);
    let from = fixture.knowledge(fixture.source).export(&id, true).unwrap();
    let to = fixture.knowledge(fixture.destination).export(&id, true).unwrap();
    assert_eq!(from.uri(), to.uri());
    assert_eq!(from.bytes.as_ref(), to.bytes.as_ref());

    // A replay converges: nothing new, no publication.
    let replay = fixture
        .transfer(
            fixture.source,
            fixture.destination,
            TransferOptions::default(),
        )
        .unwrap();
    assert_eq!(replay.publications, 0);
    assert_eq!(replay.new_knowledge_facts, 0);
    assert!(replay.verified);
    assert_eq!(
        census(&fixture.knowledge_facts(fixture.destination)),
        census(&destination)
    );
}

#[test]
fn opt_ins_carry_history_links_and_the_linked_file_but_nothing_unrelated() {
    let fixture = Fixture::new();
    // First the revision alone, then everything: the second run adds only
    // what the first left out.
    fixture
        .transfer(
            fixture.source,
            fixture.destination,
            TransferOptions::default(),
        )
        .unwrap();
    let all = TransferOptions {
        history: true,
        links: true,
        files: true,
        dry_run: false,
    };
    let report = fixture
        .transfer(fixture.source, fixture.destination, all)
        .unwrap();
    assert_eq!(
        carried(&report),
        BTreeSet::from([
            (fixture.guide, Reason::Requested),
            (fixture.private_parent, Reason::History),
            (fixture.linked, Reason::Linked),
            (fixture.attachment, Reason::Attachment),
        ])
    );
    let guide = report
        .carried
        .iter()
        .find(|item| item.id == fixture.guide)
        .unwrap();
    assert_eq!(guide.new_facts, 0, "already transferred");
    assert_eq!(report.publications, 2);
    assert!(reference(&report, ReferenceKind::Parent).is_empty());
    assert!(reference(&report, ReferenceKind::Link).is_empty());
    assert!(reference(&report, ReferenceKind::File).is_empty());
    assert_eq!(reference(&report, ReferenceKind::ProvenanceBundle).len(), 1);

    let files = fixture.files_ids(fixture.destination);
    assert_eq!(files, BTreeSet::from([fixture.attachment]));
    assert!(!files.contains(&fixture.unrelated) && !files.contains(&fixture.bundle));
    let export = crate::files::Files::with_storage(fixture.storage(fixture.destination).unwrap())
        .get(&format!("{:x}", fixture.attachment))
        .unwrap();
    assert_eq!(export.bytes.as_ref(), ATTACHMENT);
    let destination = fixture.knowledge_facts(fixture.destination);
    assert!(!knowledge_model::revision_records(&destination, fixture.private_parent).is_empty());

    let replay = fixture
        .transfer(fixture.source, fixture.destination, all)
        .unwrap();
    assert_eq!(
        (
            replay.publications,
            replay.new_knowledge_facts,
            replay.new_file_facts
        ),
        (0, 0, 0)
    );
    let _ = (
        fixture.source_knowledge,
        fixture.source_files,
        fixture.destination_files,
    );
}

#[test]
fn transfer_preserves_an_older_articles_original_source_citation() {
    let fixture = Fixture::new();
    let original = crate::files::stage(
        Bytes::from(b"Original page.".to_vec()),
        "Guide.md",
        "text/plain",
    )
    .unwrap();
    let file_id = original.root().unwrap();
    let content = find!(handle: ContentHandle, pattern!(original.facts(), [{ file_id @ file::content: ?handle }])).next().unwrap();
    crate::files::Files::with_storage(fixture.storage(fixture.source).unwrap())
        .publish(original)
        .unwrap();
    let facts = fixture.knowledge_facts(fixture.source);
    let origin = find!(id: Id, pattern!(&facts, [{
        ?id @ metadata::tag: &schema::KIND_SOURCE_PROVENANCE, schema::attrs::revision: &fixture.guide,
    }])).next().unwrap();
    fixture.commit_source(entity! { ExclusiveId::force_ref(&origin) @ schema::attrs::references_file_content: content });
    let opts = TransferOptions {
        files: true,
        ..Default::default()
    };
    fixture
        .transfer(fixture.source, fixture.destination, opts)
        .unwrap();
    assert!(fixture.files_ids(fixture.destination).contains(&file_id));
    let displayed = fixture
        .knowledge(fixture.destination)
        .show(&format!("{:x}", fixture.guide), true)
        .unwrap();
    assert!(displayed.contains(&format!(
        "#link(\"files:{}\")[Originalquelle]",
        content_hash_hex(content)
    )));
    assert_eq!(
        fixture
            .transfer(fixture.source, fixture.destination, opts)
            .unwrap()
            .publications,
        0
    );
}

#[test]
fn same_root_converges_without_publication() {
    let fixture = Fixture::new();
    let report = fixture
        .transfer(
            fixture.source,
            fixture.same_root,
            TransferOptions {
                history: true,
                links: true,
                files: true,
                dry_run: false,
            },
        )
        .unwrap();
    assert!(report.same_root());
    assert_eq!(
        (
            report.publications,
            report.new_knowledge_facts,
            report.new_file_facts
        ),
        (0, 0, 0)
    );
    assert!(report.verified);
}

#[test]
fn missing_workspaces_revisions_and_roles_refuse_before_publication() {
    let fixture = Fixture::new();
    assert!(
        fixture.storage(genid().id).is_err(),
        "unknown destination workspace"
    );
    let error = fixture
        .knowledge(fixture.source)
        .transfer(
            &fixture.knowledge(fixture.destination),
            genid().id,
            TransferOptions::default(),
        )
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("not a revision in the source"),
        "{error:#}"
    );
    let error = fixture
        .knowledge(fixture.destination)
        .transfer(
            &fixture.knowledge(fixture.source),
            fixture.guide,
            TransferOptions::default(),
        )
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("not a revision in the source"),
        "{error:#}"
    );

    let before = census(&fixture.knowledge_facts(fixture.knowledge_only));
    let error = fixture
        .transfer(
            fixture.source,
            fixture.knowledge_only,
            TransferOptions {
                files: true,
                ..Default::default()
            },
        )
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("has no Files role"),
        "{error:#}"
    );
    assert_eq!(census(&fixture.knowledge_facts(fixture.knowledge_only)), before);

    // Without files the Knowledge-only workspace accepts the revision, and says
    // it could not look for the attachment.
    let report = fixture
        .transfer(
            fixture.source,
            fixture.knowledge_only,
            TransferOptions::default(),
        )
        .unwrap();
    assert_eq!(
        reference(&report, ReferenceKind::File)[0].at_destination,
        Presence::Unknown
    );
}

#[test]
fn a_failed_or_partial_transfer_converges_on_retry() {
    let fixture = Fixture::new();
    let all = TransferOptions {
        history: false,
        links: false,
        files: true,
        dry_run: false,
    };
    // A failing first commit publishes nothing at all.
    publish_fault::arm(publish_fault::Fault::BeforeCommit);
    assert!(fixture
        .transfer(fixture.source, fixture.destination, all)
        .is_err());
    assert!(fixture.files_ids(fixture.destination).is_empty());
    assert!(fixture
        .knowledge(fixture.destination)
        .show(&format!("{:x}", fixture.guide), true)
        .is_err());

    // A run that stopped after its Files commit: the attachment is there,
    // the revision is not. The retry adds only the revision.
    let staged =
        crate::files::stage(Bytes::from(ATTACHMENT.to_vec()), "diagram.png", "image/png").unwrap();
    crate::files::Files::with_storage(fixture.storage(fixture.destination).unwrap())
        .publish(staged)
        .unwrap();
    let report = fixture
        .transfer(fixture.source, fixture.destination, all)
        .unwrap();
    let attachment = report
        .carried
        .iter()
        .find(|item| item.id == fixture.attachment)
        .unwrap();
    assert_eq!(attachment.new_facts, 0, "the attachment was already there");
    assert_eq!(report.new_file_facts, 0);
    assert_eq!(report.publications, 1);
    assert!(report.verified);
}

#[test]
fn the_cli_binds_from_and_to_and_refuses_an_ambient_workspace() {
    let fixture = Fixture::new();
    let run = |extra: &[String]| {
        let mut text = String::new();
        let mut arguments = vec![
            "wiki".to_owned(),
            "--pile".to_owned(),
            fixture.pile.display().to_string(),
            "--key".to_owned(),
            fixture.key.display().to_string(),
        ];
        arguments.extend(extra.iter().cloned());
        let result = super::super::cli::execute_from(
            arguments,
            &mut crate::out::Out::new(&mut |part| {
                if let crate::out::Part::Text { text: next } = part {
                    text.push_str(&next);
                }
                Ok(())
            }),
        );
        result.map(|()| text)
    };
    let transfer = |more: &[&str]| {
        let mut arguments = vec![
            "transfer".to_owned(),
            format!("{:x}", fixture.guide),
            "--from".to_owned(),
            format!("{:X}", fixture.source),
            "--to".to_owned(),
            format!("{:X}", fixture.destination),
        ];
        arguments.extend(more.iter().map(|value| value.to_string()));
        arguments
    };
    let preview = run(&transfer(&["--dry-run"])).unwrap();
    assert!(preview.contains("dry run, nothing published"), "{preview}");
    assert!(
        preview.contains(&format!("parent {:x}", fixture.private_parent)),
        "{preview}"
    );
    assert!(!preview.contains(PRIVATE_NOTE), "{preview}");
    let applied = run(&transfer(&[])).unwrap();
    assert!(applied.contains("published 1 commit(s)"), "{applied}");
    let replay = run(&transfer(&[])).unwrap();
    assert!(replay.contains("nothing new to publish"), "{replay}");

    let mut ambient = vec!["--workspace".to_owned(), format!("{:X}", fixture.source)];
    ambient.extend(transfer(&[]));
    assert!(run(&ambient).is_err());
    let mut prefix = transfer(&[]);
    prefix[1].truncate(8);
    assert!(run(&prefix).is_err(), "a prefix is not an exact revision");
}

#[test]
fn a_random_id_and_a_later_annotation_cross_as_received() {
    let fixture = Fixture::new();
    let (author_facts, author) = knowledge_model::author_record(&fixture.original.verifying_key());
    let (tag_facts, tag, _) = knowledge_model::tag_record("team-safe").unwrap();
    let random = genid();
    let id = random.id;
    let mut fragment = author_facts;
    fragment += tag_facts.clone();
    let title: schema::TextHandle = fragment.put("Random identity".to_owned());
    let content: schema::TextHandle = fragment.put("Readable, never derived.".to_owned());
    fragment += entity! { &random @
        metadata::tag: schema::KIND_REVISION,
        schema::attrs::title: title,
        schema::attrs::content: content,
        schema::attrs::author: author,
    };
    // An annotation asserted after the guide was authored.
    fragment += entity! { ExclusiveId::force_ref(&fixture.guide) @ metadata::tag: tag };
    fixture.commit_source(fragment);

    let source = fixture.knowledge(fixture.source);
    let destination = fixture.knowledge(fixture.destination);
    let report = source
        .transfer(&destination, id, TransferOptions::default())
        .unwrap();
    assert!(report.verified);
    let report = fixture
        .transfer(
            fixture.source,
            fixture.destination,
            TransferOptions::default(),
        )
        .unwrap();
    assert!(report.verified);
    let at_source = fixture.knowledge_facts(fixture.source);
    let at_destination = fixture.knowledge_facts(fixture.destination);
    for revision in [id, fixture.guide] {
        let records = knowledge_model::revision_records(&at_destination, revision);
        assert!(!records.is_empty());
        assert_eq!(knowledge_model::revision_records(&at_source, revision), records);
    }
    assert!(
        knowledge_model::revision_records(&at_destination, fixture.guide)[0]
            .tags
            .contains(&tag)
    );
    let shown = destination
        .show(&format!("{:x}", fixture.guide), true)
        .unwrap();
    assert!(shown.contains("team-safe"), "{shown}");
}
