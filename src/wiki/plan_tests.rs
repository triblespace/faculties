//! `wiki import --plan`: replaying a translated foreign history converges.
//!
//! The fixture mirrors a small Gollum repository: three commits by three
//! authors, an attachment, links in both directions inside one commit (broken
//! by a fixup revision), an edit, and a rename.

use super::*;
use crate::schemas::files::KIND_IMPORT;
use crate::storage::initialize_signer;
use anybytes::Bytes;
use std::fs::File;

const BUNDLE_NAME: &str = "history.bundle";

struct Pile {
    _directory: tempfile::TempDir,
    storage: Storage,
}

impl Pile {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let pile = directory.path().join("wiki.pile");
        let key = directory.path().join("wiki.key");
        File::create(&pile).unwrap();
        initialize_signer(&pile, Some(&key)).unwrap();
        Self {
            storage: Storage::new(pile, Some(key)),
            _directory: directory,
        }
    }

    /// A second, empty pile written by the same importer key.
    fn sibling(&self) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let pile = directory.path().join("wiki.pile");
        let key = directory.path().join("wiki.key");
        File::create(&pile).unwrap();
        fs::copy(self.storage.key_path().unwrap(), &key).unwrap();
        Self {
            storage: Storage::new(pile, Some(key)),
            _directory: directory,
        }
    }

    fn wiki(&self) -> Wiki {
        Wiki::with_storage(self.storage.clone())
    }

    fn files(&self) -> crate::files::Files {
        crate::files::Files::with_storage(self.storage.clone())
    }

    fn storage(&self) -> WikiStorage<'_> {
        WikiStorage {
            storage: &self.storage,
        }
    }

    /// The git bundle as Files holds it, the way the migration stores it.
    fn add_bundle(&self) -> Id {
        self.files()
            .add_bytes(
                Bytes::from(b"# v2 git bundle\nsynthetic history\n".to_vec()),
                BUNDLE_NAME,
                "application/octet-stream",
                &[],
            )
            .unwrap()
    }

    fn counts(&self) -> Counts {
        self.storage()
            .view_with_scope(FILES_SCOPE_ID, "Files", |view, files| {
                let tagged = |facts: &FactArchive, kind: &Id| {
                    find!(id: Id, pattern!(facts, [{ ?id @ metadata::tag: kind }])).count()
                };
                let entries = wiki_model::entries(&view.facts, &view.latest);
                Ok(Counts {
                    entries: entries.len(),
                    heads: entries.iter().map(|entry| entry.frontier.len()).sum(),
                    revisions: all_selectors(&view.facts).len(),
                    authorships: tagged(&view.facts, &schema::KIND_AUTHORSHIP),
                    provenance: tagged(&view.facts, &schema::KIND_SOURCE_PROVENANCE),
                    files: tagged(files, &KIND_FILE),
                    file_imports: tagged(files, &KIND_IMPORT),
                })
            })
            .unwrap()
    }

    fn entry_of(&self, revision: Id) -> EntryRecord {
        self.storage()
            .view(|view| Ok(wiki_model::entry(&view.facts, &view.latest, revision).unwrap()))
            .unwrap()
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Counts {
    entries: usize,
    heads: usize,
    revisions: usize,
    authorships: usize,
    provenance: usize,
    files: usize,
    file_imports: usize,
}

const FLOOR_PNG: &[u8] = b"\x89PNG\r\n\x1a\nsynthetic floor plan";

fn revision(
    key: &str,
    page: &str,
    path: &str,
    title: &str,
    content: &str,
    predecessors: &[&str],
) -> PlanRevision {
    PlanRevision {
        key: key.into(),
        page: page.into(),
        path: path.into(),
        title: title.into(),
        content: content.into(),
        predecessors: predecessors.iter().map(|key| (*key).to_owned()).collect(),
    }
}

fn date(raw: &str) -> Inline<inlineencodings::NsTAIInterval> {
    crate::wiki::cli::parse_source_date(raw).unwrap()
}

#[test]
fn a_source_date_is_its_instant_whatever_the_offset() {
    assert_eq!(
        date("2019-03-01T10:00:00+01:00"),
        date("2019-03-01T09:00:00Z")
    );
    assert_ne!(
        date("2019-03-01T10:00:00+01:00"),
        date("2019-03-01T10:00:00Z")
    );
    assert!(crate::wiki::cli::parse_source_date("2019-03-01").is_err());
}

fn fixture_plan(bundle: Option<Id>) -> ImportPlan {
    ImportPlan {
        bundle,
        commits: vec![
            PlanCommit {
                commit: "4b6be2022add6d5322b26a85e2ab440e8812c267".into(),
                author: "Anna Admin <anna@example.test>".into(),
                authored_at: Some(date("2019-03-01T10:00:00+01:00")),
                attachments: vec![PlanAttachment {
                    key: "uploads/floor.png".into(),
                    path: "uploads/floor.png".into(),
                    media_type: "image/png".into(),
                    bytes: Bytes::from(FLOOR_PNG.to_vec()),
                }],
                revisions: vec![
                    revision("c1:guide", "guide", "Onboarding-Guide.md", "Onboarding Guide",
                        "= Onboarding Guide\n\n+ Get a badge.\n", &[]),
                    revision("c1:office", "office", "Office-Plan.md", "Office Plan",
                        "= Office Plan\n\nSee #link(\"plan:uploads/floor.png\")[the floor plan].\n", &[]),
                    revision("c1:home", "home", "Home.md", "Home",
                        "= Home\n\nStart with #link(\"plan:guide\")[the guide] and #link(\"plan:office\")[the office].\n", &[]),
                    // The fixup: the guide links back to Home, which did not
                    // exist when the guide's first revision was minted.
                    revision("c1:guide:fixup", "guide", "Onboarding-Guide.md", "Onboarding Guide",
                        "= Onboarding Guide\n\nBack to #link(\"plan:home\")[Home].\n\n+ Get a badge.\n", &["c1:guide"]),
                ],
            },
            PlanCommit {
                commit: "eb84fc2f6006ef4c72da878250b00179b6b0d633".into(),
                author: "Ben Builder <ben@example.test>".into(),
                authored_at: Some(date("2019-04-02T14:30:00+02:00")),
                attachments: vec![],
                revisions: vec![revision("c2:guide", "guide", "Onboarding-Guide.md", "Onboarding Guide",
                    "= Onboarding Guide\n\nBack to #link(\"plan:home\")[Home].\n\n+ Get a badge.\n+ Meet the team.\n", &["c1:guide:fixup"])],
            },
            PlanCommit {
                commit: "d9454cdf2b61e14effc408a9b5175adc5cffdfb6".into(),
                author: "Clara Curator <clara@example.test>".into(),
                authored_at: Some(date("2020-01-15T09:00:00+01:00")),
                attachments: vec![],
                revisions: vec![
                    revision("c3:guide", "guide", "New-Staff-Guide.md", "New Staff Guide",
                        "= New Staff Guide\n\nBack to #link(\"plan:home\")[Home].\n\n+ Get a badge.\n+ Meet the team.\n", &["c2:guide"]),
                    revision("c3:home", "home", "Home.md", "Home",
                        "= Home\n\nStart with #link(\"plan:guide\")[the staff guide] and #link(\"plan:office\")[the office].\n", &["c1:home"]),
                ],
            },
        ],
    }
}

fn ids(report: &PlanReport) -> BTreeMap<String, Id> {
    report.revisions.iter().cloned().collect()
}

#[test]
fn replaying_a_plan_adds_nothing() {
    let pile = Pile::new();
    let bundle = pile.add_bundle();
    let first = pile
        .wiki()
        .import_plan(fixture_plan(Some(bundle)), &[])
        .unwrap();
    assert_eq!(first.new_revisions, 7);
    assert_eq!(first.new_provenance, 7);
    assert_eq!(first.new_files, 1);
    let before = pile.counts();
    assert_eq!(
        before,
        Counts {
            entries: 3,
            heads: 3,
            revisions: 7,
            authorships: 7,
            provenance: 7,
            files: 2,
            file_imports: 2
        }
    );

    let replay = pile
        .wiki()
        .import_plan(fixture_plan(Some(bundle)), &[])
        .unwrap();
    assert_eq!(ids(&replay), ids(&first), "a replay mints the same ids");
    assert_eq!(
        (
            replay.new_revisions,
            replay.new_provenance,
            replay.new_files,
            replay.publications
        ),
        (0, 0, 0, 0)
    );
    assert_eq!(pile.counts(), before);
}

#[test]
fn an_interrupted_run_then_a_rerun_converges_on_the_full_run() {
    let full = Pile::new();
    let expected = full.wiki().import_plan(fixture_plan(None), &[]).unwrap();

    let resumed = full.sibling();
    // Interrupted between the attachment commit and the first page commit.
    let mut plan = fixture_plan(None);
    plan.commits.truncate(1);
    plan.commits[0].revisions.clear();
    resumed.wiki().import_plan(plan, &[]).unwrap();
    // Interrupted again after the second source commit.
    let mut plan = fixture_plan(None);
    plan.commits.truncate(2);
    let partial = resumed.wiki().import_plan(plan, &[]).unwrap();
    assert_eq!(partial.new_files, 0, "the attachment is not imported twice");
    let rerun = resumed.wiki().import_plan(fixture_plan(None), &[]).unwrap();

    assert_eq!(ids(&rerun), ids(&expected));
    assert_eq!(
        rerun.new_revisions, 2,
        "only the third commit's revisions are new"
    );
    assert_eq!(resumed.counts(), full.counts());
}

#[test]
fn a_native_edit_after_the_import_survives_a_replay() {
    let pile = Pile::new();
    let report = pile.wiki().import_plan(fixture_plan(None), &[]).unwrap();
    let home = ids(&report)["c3:home"];
    let native = pile
        .wiki()
        .edit(
            &format!("{home:x}"),
            Some("= Home\n\nEdited after the migration.\n"),
            None,
            &[],
            false,
        )
        .unwrap();
    let before = pile.counts();
    let replay = pile.wiki().import_plan(fixture_plan(None), &[]).unwrap();
    assert_eq!(replay.publications, 0);
    assert_eq!(pile.counts(), before);
    let entry = pile.entry_of(home);
    assert_eq!(
        entry
            .frontier
            .iter()
            .map(|head| head.id)
            .collect::<Vec<_>>(),
        vec![native]
    );
}

#[test]
fn a_rename_is_one_entry_with_its_whole_history() {
    let pile = Pile::new();
    let report = pile.wiki().import_plan(fixture_plan(None), &[]).unwrap();
    let ids = ids(&report);
    let entry = pile.entry_of(ids["c1:guide"]);
    assert_eq!(entry.roots, vec![ids["c1:guide"]]);
    assert_eq!(entry.members.len(), 4);
    assert_eq!(
        entry
            .frontier
            .iter()
            .map(|head| head.id)
            .collect::<Vec<_>>(),
        vec![ids["c3:guide"]]
    );
    let title = pile
        .storage()
        .view(|view| revision_title(&view.reader, &entry.frontier[0]))
        .unwrap();
    assert_eq!(title, "New Staff Guide");
    let history = pile
        .storage()
        .view(|view| Ok(wiki_model::entry_history(&view.facts, &entry)))
        .unwrap();
    assert_eq!(
        history
            .iter()
            .map(|revision| revision.id)
            .collect::<Vec<_>>(),
        ["c1:guide", "c1:guide:fixup", "c2:guide", "c3:guide"].map(|key| ids[key]),
    );
}

#[test]
fn links_name_the_pages_and_files_as_they_stood_and_resolve_after_replay() {
    let pile = Pile::new();
    let report = pile.wiki().import_plan(fixture_plan(None), &[]).unwrap();
    pile.wiki().import_plan(fixture_plan(None), &[]).unwrap();
    let ids = ids(&report);
    let (home, office, breakage) = pile
        .storage()
        .view(|view| {
            let read = |key: &str| {
                let record = wiki_model::revision_records(&view.facts, ids[key]).remove(0);
                revision_content(&view.reader, &record)
            };
            let model = FrontierModel::load(&view.reader, &view.facts, &view.latest)?;
            Ok((
                read("c3:home")?,
                read("c1:office")?,
                model.audit().breakage(),
            ))
        })
        .unwrap();
    assert!(
        home.contains(&format!("wiki:{:x}", ids["c3:guide"])),
        "{home}"
    );
    assert!(
        home.contains(&format!("wiki:{:x}", ids["c1:office"])),
        "{home}"
    );
    let hash = blake3::hash(FLOOR_PNG).to_hex().to_string();
    assert!(office.contains(&format!("files:{hash}")), "{office}");
    assert!(!home.contains("plan:") && !office.contains("plan:"));
    assert_eq!(breakage, 0);
}

#[test]
fn attachment_bytes_read_back_and_a_replay_is_no_new_import() {
    let pile = Pile::new();
    pile.wiki().import_plan(fixture_plan(None), &[]).unwrap();
    let hash = blake3::hash(FLOOR_PNG).to_hex().to_string();
    assert_eq!(pile.files().get(&hash).unwrap().bytes.as_ref(), FLOOR_PNG);
    let before = pile.counts();
    // The same bytes added again through Files are a second import
    // occurrence of one file, not duplicate bytes.
    pile.files()
        .add_bytes(
            Bytes::from(FLOOR_PNG.to_vec()),
            "floor.png",
            "image/png",
            &[],
        )
        .unwrap();
    let again = pile.counts();
    assert_eq!(
        (again.files, again.file_imports),
        (before.files, before.file_imports + 1)
    );
    pile.wiki().import_plan(fixture_plan(None), &[]).unwrap();
    assert_eq!(
        pile.counts(),
        again,
        "a replay neither re-imports nor duplicates"
    );
}

#[test]
fn every_revision_carries_its_source_commit_path_author_and_date() {
    let pile = Pile::new();
    let bundle = pile.add_bundle();
    let report = pile
        .wiki()
        .import_plan(fixture_plan(Some(bundle)), &[])
        .unwrap();
    let ids = ids(&report);
    type Text = schema::TextHandle;
    let rows = pile
        .storage()
        .view(|view| {
            let mut rows = Vec::new();
            for (revision, commit, path, author, at, cited) in find!(
                (revision: Id, commit: Text, path: Text, author: Text, at: Inline<inlineencodings::NsTAIInterval>, cited: Id),
                pattern!(&view.facts, [{
                    _?record @
                    metadata::tag: &schema::KIND_SOURCE_PROVENANCE,
                    schema::attrs::revision: ?revision,
                    crate::schemas::code::attrs::commit: ?commit,
                    file::source_path: ?path,
                    crate::schemas::archive::import_schema::source_author: ?author,
                    crate::schemas::archive::import_schema::source_created_at: ?at,
                    schema::attrs::references_file: ?cited,
                }])
            ) {
                rows.push((
                    revision,
                    read_string(&view.reader, commit)?,
                    read_string(&view.reader, path)?,
                    read_string(&view.reader, author)?,
                    at,
                    cited,
                ));
            }
            Ok(rows)
        })
        .unwrap();
    assert_eq!(rows.len(), 7);
    let renamed = rows.iter().find(|row| row.0 == ids["c3:guide"]).unwrap();
    assert_eq!(renamed.1, "d9454cdf2b61e14effc408a9b5175adc5cffdfb6");
    assert_eq!(renamed.2, "New-Staff-Guide.md");
    assert_eq!(renamed.3, "Clara Curator <clara@example.test>");
    assert_eq!(renamed.4, date("2020-01-15T09:00:00+01:00"));
    assert_eq!(renamed.5, bundle);
    let edited = rows.iter().find(|row| row.0 == ids["c2:guide"]).unwrap();
    assert_eq!(edited.3, "Ben Builder <ben@example.test>");

    // The importer is every revision's author: the git authors are text.
    let (_, importer) = pile.storage().author_fragment().unwrap();
    let authors = pile
        .storage()
        .view(|view| {
            Ok(ids
                .values()
                .flat_map(|id| wiki_model::revision_records(&view.facts, *id))
                .map(|record| record.author)
                .collect::<BTreeSet<_>>())
        })
        .unwrap();
    assert_eq!(authors, BTreeSet::from([Some(importer)]));
}

#[test]
fn a_plan_that_points_forward_or_across_pages_fails_before_writing() {
    let pile = Pile::new();
    let mut forward = fixture_plan(None);
    forward.commits[0].revisions.swap(0, 2);
    let error = pile.wiki().import_plan(forward, &[]).unwrap_err();
    assert!(
        format!("{error:#}").contains("plan key 'guide'"),
        "{error:#}"
    );

    let mut across = fixture_plan(None);
    across.commits[2].revisions[1].predecessors = vec!["c2:guide".into()];
    let error = pile.wiki().import_plan(across, &[]).unwrap_err();
    assert!(
        format!("{error:#}").contains("on page 'guide'"),
        "{error:#}"
    );
    assert_eq!(pile.counts().revisions, 0);
}

#[test]
fn the_cli_reads_a_plan_file_and_replays_it() {
    let pile = Pile::new();
    let directory = tempfile::tempdir().unwrap();
    let base = directory.path();
    fs::create_dir_all(base.join("pages")).unwrap();
    fs::write(
        base.join("pages/home.typ"),
        "= Home\n\nSee #link(\"plan:uploads/a.png\")[a].\n",
    )
    .unwrap();
    fs::write(base.join("a.png"), FLOOR_PNG).unwrap();
    let plan = serde_json::json!({
        "commits": [{
            "commit": "4b6be2022add6d5322b26a85e2ab440e8812c267",
            "author": "Anna Admin <anna@example.test>",
            "date": "2019-03-01T10:00:00+01:00",
            "attachments": [{ "path": "uploads/a.png", "file": "a.png" }],
            "revisions": [{ "key": "c1:home", "page": "home", "path": "Home.md",
                            "title": "Home", "content": "pages/home.typ" }]
        }]
    });
    let plan_path = base.join("plan.json");
    fs::write(&plan_path, serde_json::to_vec(&plan).unwrap()).unwrap();

    let run = || {
        let mut lines = Vec::new();
        crate::wiki::cli::execute_from(
            [
                "wiki",
                "--pile",
                pile.storage.path().to_str().unwrap(),
                "--key",
                pile.storage.key_path().unwrap().to_str().unwrap(),
                "import",
                "--plan",
                plan_path.to_str().unwrap(),
            ],
            &mut Out::new(&mut |part| {
                if let crate::out::Part::Text { text } = part {
                    lines.push(text);
                }
                Ok(())
            }),
        )
        .unwrap();
        lines.concat()
    };
    let first = run();
    assert!(
        first.contains("1 new revisions") && first.contains("1 new files"),
        "{first}"
    );
    let second = run();
    assert!(
        second.contains("0 new revisions") && second.contains("0 publications"),
        "{second}"
    );
    let authored = pile
        .storage()
        .view(|view| {
            Ok(find!(
                at: Inline<inlineencodings::NsTAIInterval>,
                pattern!(&view.facts, [{
                    _?record @ crate::schemas::archive::import_schema::source_created_at: ?at
                }])
            )
            .collect::<Vec<_>>())
        })
        .unwrap();
    assert_eq!(authored, vec![date("2019-03-01T10:00:00+01:00")]);
}
