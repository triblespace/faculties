use anybytes::Bytes;
use base64::Engine as _;
use clap::Parser;
use faculties::archive::{cli, mcp, Archive, ImportSource};
use faculties::mcp::{Faculty, InvalidArguments, Server};
use faculties::out::{Out, Part};
use faculties::schemas::blockdag as schema;
use faculties::storage::FactView;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use triblespace::core::metadata;
use triblespace::prelude::*;

struct Fixture {
    directory: tempfile::TempDir,
    pile: PathBuf,
    key: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let pile = directory.path().join("archive.pile");
        let key = directory.path().join("explicit.key");
        fs::File::create(&pile).unwrap();
        faculties::storage::initialize_signer(&pile, Some(&key)).unwrap();
        Self {
            directory,
            pile,
            key,
        }
    }
    fn archive(&self) -> Archive {
        Archive::new(self.pile.clone(), Some(self.key.clone()))
    }
    fn adapter(&self) -> mcp::Archive {
        mcp::Archive::new(self.pile.clone(), Some(self.key.clone()))
    }
    fn call(&self, tool: &str, args: Value) -> Vec<Part> {
        let mut parts = Vec::new();
        self.adapter()
            .call(
                tool,
                serde_json::to_vec(&args).unwrap().into(),
                &mut Out::new(&mut |p| {
                    parts.push(p);
                    Ok(())
                }),
            )
            .unwrap();
        parts
    }
    fn cli(&self, args: &[&str]) -> Vec<Part> {
        let mut argv = vec![
            "archive",
            "--pile",
            self.pile.to_str().unwrap(),
            "--key",
            self.key.to_str().unwrap(),
        ];
        argv.extend_from_slice(args);
        let cli = cli::Cli::try_parse_from(argv).unwrap();
        let mut parts = Vec::new();
        cli::execute(
            cli,
            &mut Out::new(&mut |p| {
                parts.push(p);
                Ok(())
            }),
        )
        .unwrap();
        parts
    }
    fn projections(&self) -> Vec<Id> {
        let observed = pollster::block_on(faculties::archive_collection::ensure_local(
            &self.pile,
            Some(&self.key),
        ))
        .unwrap();
        let facts = observed.facts().unwrap();
        find!(projection:Id,pattern!(&facts,[{?projection @ metadata::tag:&schema::source_projection::KIND}])).collect()
    }
    fn roots(&self) -> usize {
        let observed = pollster::block_on(faculties::archive_collection::ensure_local(
            &self.pile,
            Some(&self.key),
        ))
        .unwrap();
        observed.support().len()
    }
}
fn text(parts: &[Part]) -> String {
    parts
        .iter()
        .map(|p| match p {
            Part::Text { text } => text.as_str(),
            _ => panic!("diagnostic text expected"),
        })
        .collect()
}
const CODE: &str = r#"{"type":"user","sessionId":"resident","uuid":"first","timestamp":"2026-03-01T15:34:01Z","message":{"role":"user","content":"quasar needle @literal"}}"#;
const CODEX: &str = concat!(
    r#"{"timestamp":"2026-08-16T08:00:00Z","type":"session_meta","payload":{"id":"resident-session","session_id":"resident-session"}}"#,
    "\n",
    r#"{"timestamp":"2026-08-16T08:01:00Z","type":"event_msg","payload":{"type":"user_message","message":"quasar needle"}}"#,
    "\n"
);
const CHATGPT: &str = r#"[{"id":"resident-chatgpt","mapping":{"node":{"id":"node","parent":null,"message":{"id":"message","author":{"role":"user"},"content":{"content_type":"text","parts":["quasar needle"]}}}}}]"#;
const WEB: &str = r#"[{"uuid":"resident-web","chat_messages":[{"uuid":"message","sender":"human","text":"quasar needle"}]}]"#;
const COPILOT: &str = r#"{"sessionId":"resident-copilot","requests":[{"requestId":"request","message":{"text":"quasar needle"},"response":[{"value":"answer"}]}]}"#;
const AGY: &str = concat!(
    r#"{"source":"USER_INPUT","content":"quasar needle","step_index":1}"#,
    "\n"
);
fn gemini(inner: &str) -> String {
    format!("<html><body><div class=\"outer-cell mdl-cell mdl-cell--12-col mdl-shadow--2dp\"><div><div class=\"header-cell\"><p>Gemini Apps<br></p></div><div class=\"content-cell mdl-cell mdl-cell--6-col mdl-typography--body-1\">{inner}</div><div class=\"content-cell mdl-cell mdl-cell--6-col mdl-typography--text-right\"></div></div></div></body></html>")
}

#[test]
fn all_seven_resident_scanners_preserve_file_projection_identity_and_bytes() {
    let html = gemini("Prompted&nbsp;quasar needle<br>18 Sept 2025, 12:02:52 CET<br><p>answer</p>");
    for (source, spelling, name, content) in [
        (ImportSource::Agy, "agy", "transcript_full.jsonl", AGY),
        (
            ImportSource::ChatGpt,
            "chatgpt",
            "conversations.json",
            CHATGPT,
        ),
        (
            ImportSource::ClaudeCode,
            "claude-code",
            "claude.jsonl",
            CODE,
        ),
        (ImportSource::ClaudeWeb, "claude-web", "web.json", WEB),
        (ImportSource::Codex, "codex", "rollout.jsonl", CODEX),
        (ImportSource::Copilot, "copilot", "copilot.json", COPILOT),
        (
            ImportSource::Gemini,
            "gemini",
            "My Activity.html",
            html.as_str(),
        ),
    ] {
        let fixture = Fixture::new();
        let path = fixture.directory.path().join(name);
        fs::write(&path, content).unwrap();
        let first = fixture.cli(&["import", path.to_str().unwrap(), "--source", spelling]);
        assert!(
            text(&first).contains("1 piece(s) in 3 COMMIT(s), "),
            "{spelling}: {}",
            text(&first)
        );
        let ids = fixture.projections();
        assert!(!ids.is_empty(), "{spelling}");
        // After the read: it carries and attaches, which appends.
        let length = fs::metadata(&fixture.pile).unwrap().len();
        let receipt = fixture
            .archive()
            .import(
                source,
                path.to_str().unwrap(),
                Bytes::from(content.to_owned()),
                &BTreeMap::new(),
            )
            .unwrap();
        assert_eq!(
            fs::metadata(&fixture.pile).unwrap().len(),
            length,
            "resident {spelling} must publish the COMMITs of its identical file projection"
        );
        assert_eq!(receipt.publication.pieces, 1, "{spelling}");
        assert_eq!(
            fixture.roots(),
            3,
            "{spelling}: the vocabulary and one piece's two COMMITs"
        );
        assert_eq!(fixture.projections(), ids);
    }
}

#[test]
fn direct_cli_and_mcp_share_frozen_queries_and_literal_content() {
    let fixture = Fixture::new();
    fixture.call(
        "archive_import",
        json!({"source":"claude-code","source_name":"@not-a-host-path","content":CODE}),
    );
    let projection = format!("{:X}", fixture.projections()[0]);
    for (args, tool, values) in [
        (vec!["list"], "archive_list", json!({})),
        (
            vec!["show", projection.as_str()],
            "archive_show",
            json!({"id":projection}),
        ),
        (
            vec!["thread", projection.as_str()],
            "archive_thread",
            json!({"id":projection}),
        ),
        (
            vec!["search", "quasar"],
            "archive_search",
            json!({"text":"quasar"}),
        ),
        (vec!["index"], "archive_index", json!({})),
    ] {
        assert_eq!(fixture.cli(&args), fixture.call(tool, values));
    }
    assert!(text(&fixture.call("archive_show", json!({"id":projection}))).contains("@literal"));
    let query = fixture.directory.path().join("query.txt");
    fs::write(&query, "quasar").unwrap();
    let at = format!("@{}", query.display());
    assert!(!fixture.cli(&["search", &at]).is_empty());
    assert!(fixture
        .call("archive_search", json!({"text":at}))
        .is_empty());
    assert!(fixture.call("archive_list", json!({"limit":0})).is_empty());
    assert!(fixture
        .call("archive_search", json!({"text":"quasar","limit":0}))
        .is_empty());
}

#[test]
fn resident_attachments_never_fall_back_to_host_files() {
    let fixture = Fixture::new();
    let source = fixture.directory.path().join("conversations.json");
    let secret = fixture.directory.path().join("file-abc-hidden.png");
    fs::write(&secret, b"host bytes must stay unread").unwrap();
    let content = r#"[{"id":"resident-media","mapping":{"n":{"id":"n","message":{"id":"m","author":{"role":"user"},"content":{"content_type":"multimodal_text","parts":[{"content_type":"image_asset_pointer","asset_pointer":"file-service://file-abc"}]}}}}}]"#;
    let mut collected = Fragment::empty();
    let summary = faculties::archive_chatgpt::project_bytes(
        source.to_str().unwrap(),
        Bytes::from(content),
        &BTreeMap::new(),
        |p| {
            collected += p.fragment;
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(summary.attachments_resolved, 0);
    let payload = Bytes::from(vec![1, 2, 3, 4]);
    let attachments = BTreeMap::from([("file-abc-resident.png".to_owned(), payload.clone())]);
    let mut resident = Fragment::empty();
    let summary = faculties::archive_chatgpt::project_bytes(
        source.to_str().unwrap(),
        Bytes::from(content),
        &attachments,
        |p| {
            resident += p.fragment;
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(summary.attachments_resolved, 1);
    let handle:Inline<inlineencodings::Handle<blobencodings::RawBytes>>=find!(h:Inline<inlineencodings::Handle<blobencodings::RawBytes>>,pattern!(&resident,[{_?fact @ schema::content_fact::resolved_to:?h}])).next().unwrap();
    let reader = resident.blobs_mut().snapshot().unwrap();
    let recovered: Bytes = reader.get(handle).unwrap();
    assert_eq!(recovered, payload);
    let html = gemini(&format!(
        "Prompted&nbsp;hello<br>18 Sept 2025, 12:02:52 CET<br><p>answer</p><img src=\"{}\">",
        secret.display()
    ));
    let summary = faculties::archive_gemini::project_bytes(
        "uploaded.html",
        Bytes::from(html.clone()),
        &BTreeMap::new(),
        |_| Ok(()),
    )
    .unwrap();
    assert_eq!(summary.assets_resolved, 0);
    let supplied = BTreeMap::from([(secret.to_str().unwrap().to_owned(), payload)]);
    let summary = faculties::archive_gemini::project_bytes(
        "uploaded.html",
        Bytes::from(html),
        &supplied,
        |_| Ok(()),
    )
    .unwrap();
    assert_eq!(summary.assets_resolved, 1);
}

#[test]
fn replay_result_failure_does_not_advance_and_equal_time_peer_is_not_skipped() {
    let fixture = Fixture::new();
    let second = CODE
        .replace("\"first\"", "\"second\"")
        .replace("quasar needle", "another event");
    let content = format!("{CODE}\n{second}\n");
    fixture
        .archive()
        .import(
            ImportSource::ClaudeCode,
            "resident.jsonl",
            Bytes::from(content),
            &BTreeMap::new(),
        )
        .unwrap();
    fixture.call(
        "archive_replay_start",
        json!({"persona":"one","from":"2026-03-01T00:00:00"}),
    );
    fixture.call(
        "archive_replay_start",
        json!({"persona":"two","from":"2026-03-01T00:00:00"}),
    );
    let mut emitted = String::new();
    let error = fixture
        .archive()
        .replay(
            "one",
            1,
            false,
            &mut Out::new(&mut |part| {
                if let Part::Text { text } = part {
                    emitted.push_str(&text);
                }
                anyhow::bail!("rejected batch")
            }),
        )
        .unwrap_err();
    assert!(format!("{error:#}").contains("rejected batch"));
    let first = fixture.call("archive_replay", json!({"persona":"one","limit":1}));
    let other = fixture.call("archive_replay", json!({"persona":"two","limit":1}));
    assert_eq!(first, other);
    assert!(text(&first).starts_with(&emitted));
    let second = fixture.call("archive_replay", json!({"persona":"one","limit":1}));
    assert_ne!(first, second);
    assert!(
        text(&fixture.call("archive_replay", json!({"persona":"one","limit":1})))
            .contains("nothing after the cursor")
    );
}

#[test]
fn malformed_import_is_not_published_and_output_failure_is_not_retried() {
    let fixture = Fixture::new();
    let malformed = format!("{CODE}\n{{invalid\n");
    assert!(fixture
        .archive()
        .import(
            ImportSource::ClaudeCode,
            "resident",
            Bytes::from(malformed),
            &BTreeMap::new()
        )
        .is_err());
    assert_eq!(fixture.roots(), 0);
    let adapter = fixture.adapter();
    let args = serde_json::to_vec(&json!({"source_name":"resident","content":CODE})).unwrap();
    let mut count = 0;
    assert!(adapter
        .call(
            "archive_import",
            args.into(),
            &mut Out::new(&mut |_| {
                count += 1;
                anyhow::bail!("reject receipt")
            })
        )
        .is_err());
    assert_eq!(count, 1);
    assert_eq!(fixture.roots(), 3);
    let length = fs::metadata(&fixture.pile).unwrap().len();
    let again = fixture
        .archive()
        .import(
            ImportSource::ClaudeCode,
            "resident",
            Bytes::from(CODE),
            &BTreeMap::new(),
        )
        .unwrap();
    assert!(!again.publication.is_empty());
    assert_eq!(fs::metadata(&fixture.pile).unwrap().len(), length);
    assert_eq!(fixture.roots(), 3);
}

#[test]
fn codex_resident_import_retains_trailing_bytes_and_mcp_binary_content() {
    let fixture = Fixture::new();
    let content = format!("{CODEX}{{\"incomplete\":");
    let data = base64::engine::general_purpose::STANDARD.encode(content.as_bytes());
    let result = fixture.call(
        "archive_import",
        json!({"source":"codex","source_name":"rollout.jsonl","data":data}),
    );
    assert!(
        text(&result).contains("trailing_bytes_ignored=14"),
        "{}",
        text(&result)
    );
    assert_eq!(fixture.projections().len(), 1);
}

#[test]
fn finite_schemas_reject_config_duplicates_invalid_shapes_and_dates_before_storage() {
    let directory = tempfile::tempdir().unwrap();
    let adapter = mcp::Archive::new(directory.path().join("absent.pile"), None);
    assert_eq!(adapter.tools().len(), 9);
    Server::new(&[&adapter]).unwrap();
    for (tool, args) in [
        (
            "archive_import",
            r#"{"source_name":"x","content":"x","data":"eA=="}"#,
        ),
        (
            "archive_import",
            r#"{"source_name":"x","data":"!invalid!"}"#,
        ),
        (
            "archive_import",
            r#"{"source_name":"x","content":"x","path":"/host/file"}"#,
        ),
        (
            "archive_import",
            r#"{"source_name":"x","source_name":"y","content":"x"}"#,
        ),
        (
            "archive_import",
            r#"{"source":"gemini","source_name":"x","content":"x","attachments":[{"name":"x","data":""},{"name":"x","data":""}]}"#,
        ),
        ("archive_list", r#"{"pile":"/host/pile"}"#),
        ("archive_index", "[]"),
        ("archive_replay", r#"{"persona":"x","limit":0}"#),
        (
            "archive_replay_start",
            r#"{"persona":"x","from":"2026-02-30T00:00:00"}"#,
        ),
        ("archive_replay_stop", r#"{"persona":""}"#),
        ("archive_thread", r#"{"id":"abc","limit":0}"#),
    ] {
        let mut emitted = 0;
        let error = adapter
            .call(
                tool,
                Bytes::from(args),
                &mut Out::new(&mut |_| {
                    emitted += 1;
                    Ok(())
                }),
            )
            .unwrap_err();
        assert!(
            error.downcast_ref::<InvalidArguments>().is_some(),
            "{tool} {args}: {error:#}"
        );
        assert_eq!(emitted, 0);
    }
}

/// A richer Claude Code session: thinking with its sealed signature, a tool
/// call, and a tool result carrying text and an image.
const CODE_RICH: &str = concat!(
    r#"{"type":"user","sessionId":"rich","uuid":"u1","parentUuid":null,"timestamp":"2026-03-01T15:34:01.542Z","message":{"role":"user","content":"hello there"}}"#,
    "\n",
    r#"{"type":"assistant","sessionId":"rich","uuid":"a1","parentUuid":"u1","timestamp":"2026-03-01T15:34:02.000Z","message":{"role":"assistant","model":"claude-opus-4","content":[{"type":"thinking","thinking":"consider it","signature":"opaque"},{"type":"text","text":"hi!"},{"type":"tool_use","id":"toolu_1","name":"Screenshot","input":{"display":1}}]}}"#,
    "\n",
    r#"{"type":"user","sessionId":"rich","uuid":"u2","parentUuid":"a1","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":[{"type":"text","text":"screenshot below"},{"type":"image","source":{"type":"base64","media_type":"IMAGE/PNG; charset=binary","data":"iVBORw=="}}]}]}}"#,
    "\n",
    r#"{"type":"assistant","sessionId":"rich","uuid":"a2","parentUuid":"u2","timestamp":"2026-03-01T15:34:05.000Z","message":{"role":"assistant","model":"claude-opus-4","content":[{"type":"text","text":"I see it"}]}}"#,
    "\n",
);

/// Every source the seven importers read, as (source, CLI spelling, file
/// name, content).
fn every_source(html: &str) -> Vec<(ImportSource, &'static str, &'static str, String)> {
    vec![
        (
            ImportSource::Agy,
            "agy",
            "transcript_full.jsonl",
            AGY.to_owned(),
        ),
        (
            ImportSource::ChatGpt,
            "chatgpt",
            "conversations.json",
            CHATGPT.to_owned(),
        ),
        (
            ImportSource::ClaudeCode,
            "claude-code",
            "claude.jsonl",
            CODE.to_owned(),
        ),
        (
            ImportSource::ClaudeCode,
            "claude-code",
            "rich.jsonl",
            CODE_RICH.to_owned(),
        ),
        (
            ImportSource::ClaudeWeb,
            "claude-web",
            "web.json",
            WEB.to_owned(),
        ),
        (
            ImportSource::Codex,
            "codex",
            "rollout.jsonl",
            CODEX.to_owned(),
        ),
        (
            ImportSource::Copilot,
            "copilot",
            "copilot.json",
            COPILOT.to_owned(),
        ),
        (
            ImportSource::Gemini,
            "gemini",
            "My Activity.html",
            html.to_owned(),
        ),
    ]
}

/// Every annotation an importer writes comes from the session's own content,
/// so it is the same in every copy: one session imported from two folders by
/// the CLI, once plainly and once with `--finished-after` as the daily ingest
/// runs it, and once more through the resident API under another name,
/// publishes the same COMMITs, and the later imports append nothing. None of
/// these sources ends on a held-back tail; see
/// `a_held_back_tail_is_a_prefix_of_the_finished_projection` for one that
/// does.
#[test]
fn a_session_from_another_folder_or_through_the_api_appends_nothing() {
    let html = gemini("Prompted&nbsp;quasar needle<br>18 Sept 2025, 12:02:52 CET<br><p>answer</p>");
    for (source, spelling, name, content) in every_source(&html) {
        let fixture = Fixture::new();
        let mut paths = Vec::new();
        for folder in ["one/place", "another/folder", "a/third/place"] {
            let directory = fixture.directory.path().join(folder);
            fs::create_dir_all(&directory).unwrap();
            let path = directory.join(name);
            fs::write(&path, &content).unwrap();
            paths.push(path);
        }
        let first = fixture.cli(&["import", paths[0].to_str().unwrap(), "--source", spelling]);
        assert!(text(&first).contains("bytes appended"), "{spelling}");
        let length = fs::metadata(&fixture.pile).unwrap().len();
        for (path, finished) in [(&paths[1], false), (&paths[2], true)] {
            let mut args = vec!["import", path.to_str().unwrap(), "--source", spelling];
            if finished {
                args.extend(["--finished-after", "0s"]);
            }
            let again = fixture.cli(&args);
            assert!(
                text(&again).contains(", 0 bytes appended to the pile"),
                "{spelling} {name} (finished {finished}): {}",
                text(&again)
            );
        }
        fixture
            .archive()
            .import(
                source,
                &format!("elsewhere/{name}"),
                Bytes::from(content.clone()),
                &BTreeMap::new(),
            )
            .unwrap();
        assert_eq!(
            fs::metadata(&fixture.pile).unwrap().len(),
            length,
            "{spelling} {name}: a later copy appended bytes"
        );
    }
}

/// A Claude Code session stopped right after a tool ran ends on a result the
/// transcript cannot show complete, so it is held back unless the import is
/// told the file is finished. The resident API never is; the daily ingest
/// always passes `--finished-after`. The held-back projection is a prefix of
/// the finished one: the finished import appends the tail, every node of the
/// held-back import is among its nodes, and a repeat of either appends
/// nothing.
#[test]
fn a_held_back_tail_is_a_prefix_of_the_finished_projection() {
    const STOPPED: &str = concat!(
        r#"{"type":"user","sessionId":"stopped","uuid":"u","parentUuid":null,"message":{"role":"user","content":"look"}}"#,
        "\n",
        r#"{"type":"assistant","sessionId":"stopped","uuid":"c","parentUuid":"u","message":{"id":"msg_1","role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Read","input":{"file":"a"}}]}}"#,
        "\n",
        r#"{"type":"user","sessionId":"stopped","uuid":"r","parentUuid":"c","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"contents of a"}]}}"#,
        "\n",
    );
    let fixture = Fixture::new();
    let path = fixture.directory.path().join("stopped.jsonl");
    fs::write(&path, STOPPED).unwrap();
    fixture
        .archive()
        .import(
            ImportSource::ClaudeCode,
            "stopped.jsonl",
            Bytes::from(STOPPED),
            &BTreeMap::new(),
        )
        .unwrap();
    let held = blocks(&fixture);
    let length = fs::metadata(&fixture.pile).unwrap().len();
    let finished = fixture.cli(&[
        "import",
        path.to_str().unwrap(),
        "--source",
        "claude-code",
        "--finished-after",
        "0s",
    ]);
    assert!(
        !text(&finished).contains(", 0 bytes appended"),
        "{}",
        text(&finished)
    );
    let flushed = blocks(&fixture);
    assert_eq!(held.len(), 2, "the result is held back");
    assert_eq!(flushed.len(), 3, "the finished import appends it");
    assert!(held.is_subset(&flushed));
    let length_after = fs::metadata(&fixture.pile).unwrap().len();
    assert!(length_after > length);
    let again = fixture.cli(&[
        "import",
        path.to_str().unwrap(),
        "--source",
        "claude-code",
        "--finished-after",
        "0s",
    ]);
    assert!(
        text(&again).contains(", 0 bytes appended"),
        "{}",
        text(&again)
    );
}

fn blocks(fixture: &Fixture) -> std::collections::BTreeSet<Id> {
    let observed = pollster::block_on(faculties::archive_collection::ensure_local(
        &fixture.pile,
        Some(&fixture.key),
    ))
    .unwrap();
    let facts = observed.facts().unwrap();
    find!(
        block: Id,
        pattern!(&facts, [{ ?block @ metadata::tag: &schema::block::KIND }])
    )
    .collect()
}

/// Fail on any attribute an importer writes outside the identity of nodes,
/// content facts and receipts, and the annotations the session's own content
/// carries. A file path, a provider's title or an importer revision is none
/// of these. `metadata::name` is allowed on media types only, whose identity
/// it is.
fn assert_content_derived(route: &str, facts: &TribleSet) {
    use faculties::schemas::files::KIND_MEDIA_TYPE;
    let allowed: Vec<Id> = vec![
        metadata::tag.id(),
        schema::block::previous.id(),
        schema::block::holds.id(),
        schema::block::timestamp.id(),
        schema::block::responds_to.id(),
        schema::content_fact::modality.id(),
        schema::content_fact::direction.id(),
        schema::content_fact::payload.id(),
        schema::content_fact::blob.id(),
        schema::content_fact::asset_pointer.id(),
        schema::content_fact::asset_namespace.id(),
        schema::content_fact::media_type.id(),
        schema::content_fact::asset_size.id(),
        schema::content_fact::resolved_to.id(),
        schema::source_projection::source_namespace.id(),
        schema::source_projection::projects_to.id(),
        schema::source_projection::raw_record.id(),
        schema::source_projection::semantic_predecessor_support.id(),
        schema::source_projection::source_timestamp.id(),
        schema::source_projection::author.id(),
        schema::source_projection::experiencer.id(),
        schema::source_projection::raw_author.id(),
        schema::source_projection::raw_role.id(),
        schema::source_projection::raw_model.id(),
        schema::source_projection::source_locator.id(),
        schema::source_projection::source_detail.id(),
        schema::source_projection::resolution.id(),
    ];
    assert!(!facts.is_empty(), "{route} projected nothing");
    let media_types: std::collections::BTreeSet<Id> = find!(
        media: Id,
        pattern!(facts, [{ ?media @ metadata::tag: &KIND_MEDIA_TYPE }])
    )
    .collect();
    for fact in facts.iter() {
        let attribute = *fact.a();
        let media_name = attribute == metadata::name.id() && media_types.contains(fact.e());
        assert!(
            allowed.contains(&attribute) || media_name,
            "{route} writes attribute {attribute:X} on {:X}",
            fact.e()
        );
    }
}

#[test]
fn importers_write_only_content_derived_attributes() {
    let html = gemini("Prompted&nbsp;quasar needle<br>18 Sept 2025, 12:02:52 CET<br><p>answer</p>");
    let attachments = BTreeMap::new();
    for (source, _, name, content) in every_source(&html) {
        let mut facts = TribleSet::new();
        let bytes = Bytes::from(content.clone());
        let name = format!("/some/folder/{name}");
        match source {
            ImportSource::Agy => faculties::archive_agy::project_bytes(&name, bytes, |p| {
                facts += p.fragment.facts().clone();
                Ok(())
            })
            .map(drop),
            ImportSource::ChatGpt => {
                faculties::archive_chatgpt::project_bytes(&name, bytes, &attachments, |p| {
                    facts += p.fragment.facts().clone();
                    Ok(())
                })
                .map(drop)
            }
            ImportSource::ClaudeCode => {
                faculties::archive_claude_code::project_bytes(&name, bytes, |p| {
                    facts += p.fragment.facts().clone();
                    Ok(())
                })
                .map(drop)
            }
            ImportSource::ClaudeWeb => {
                faculties::archive_claude_web::project_bytes(&name, bytes, |p| {
                    facts += p.fragment.facts().clone();
                    Ok(())
                })
                .map(drop)
            }
            ImportSource::Codex => faculties::archive_codex::project_bytes(&name, bytes, |p| {
                facts += p.fragment.facts().clone();
                Ok(())
            })
            .map(drop),
            ImportSource::Copilot => faculties::archive_copilot::project_bytes(&name, bytes, |p| {
                facts += p.fragment.facts().clone();
                Ok(())
            })
            .map(drop),
            ImportSource::Gemini => {
                faculties::archive_gemini::project_bytes(&name, bytes, &attachments, |p| {
                    facts += p.fragment.facts().clone();
                    Ok(())
                })
                .map(drop)
            }
        }
        .unwrap();
        assert_content_derived(&name, &facts);
    }
}

/// A Claude Code session that carries both kinds of title record.
const CODE_TITLED: &str = concat!(
    r#"{"type":"user","sessionId":"titled","uuid":"t1","parentUuid":null,"timestamp":"2026-03-01T15:34:01Z","message":{"role":"user","content":"titled question"}}"#,
    "\n",
    r#"{"type":"assistant","sessionId":"titled","uuid":"t2","parentUuid":"t1","timestamp":"2026-03-01T15:34:02Z","message":{"role":"assistant","model":"claude-opus-4","content":[{"type":"text","text":"titled answer"}]}}"#,
    "\n",
    r#"{"type":"ai-title","sessionId":"titled","aiTitle":"An AI title"}"#,
    "\n",
    r#"{"type":"custom-title","sessionId":"titled","customTitle":"My own title"}"#,
    "\n",
);

/// A ChatGPT export with a provider title and an image attachment whose
/// sidecar file ships beside it.
const CHATGPT_TITLED: &str = r#"[{"id":"titled-chatgpt","title":"A provider title","mapping":{
  "root":{"id":"root","parent":null,"message":null},
  "user-node":{"id":"user-node","parent":"root","message":{"id":"message-user","author":{"role":"user"},"create_time":1772382841.5,"content":{"content_type":"text","parts":["hello"]}}},
  "assistant-node":{"id":"assistant-node","parent":"user-node","message":{"id":"message-assistant","author":{"role":"assistant"},"create_time":1772382842,"content":{"content_type":"multimodal_text","parts":["hi!",{"content_type":"image_asset_pointer","asset_pointer":"file-service://file-abc","size_bytes":4}]},"metadata":{"model_slug":"gpt-test","attachments":[{"id":"file-abc","name":"picture.png","mime_type":"image/png","size":4}]}}}
}}]"#;

/// The routes `project_bytes` does not take: whole directories, subagent
/// transcripts with their `meta.json`, `history.jsonl`, the finished route
/// the daily ingest uses, sidecar and attachment files beside an export, and
/// attachment maps. Every fixture that can carry a title carries one.
#[test]
fn every_import_route_writes_only_content_derived_attributes() {
    let root = tempfile::tempdir().unwrap();
    let finished = Some(std::time::SystemTime::now() + std::time::Duration::from_secs(3600));
    let collect = |route: &str, run: &mut dyn FnMut(&mut TribleSet)| {
        let mut facts = TribleSet::new();
        run(&mut facts);
        assert_content_derived(route, &facts);
    };

    // Claude Code: a project directory with a titled session and a subagent
    // with its spawn metadata, and the prompt log beside the projects.
    let claude = root.path().join("claude");
    let project = claude.join("projects/p");
    let subagents = project.join("titled/subagents");
    fs::create_dir_all(&subagents).unwrap();
    let session = project.join("titled.jsonl");
    fs::write(&session, CODE_TITLED).unwrap();
    let agent = subagents.join("agent-a1.jsonl");
    fs::write(
        &agent,
        concat!(
            r#"{"type":"user","sessionId":"titled","agentId":"a1","isSidechain":true,"uuid":"q","message":{"role":"user","content":"audit this"}}"#,
            "\n",
            r#"{"type":"assistant","sessionId":"titled","agentId":"a1","isSidechain":true,"uuid":"a","parentUuid":"q","message":{"role":"assistant","content":"done"}}"#,
            "\n",
        ),
    )
    .unwrap();
    fs::write(
        subagents.join("agent-a1.meta.json"),
        r#"{"description":"Audit the code","agentType":"general-purpose"}"#,
    )
    .unwrap();
    let history = claude.join("history.jsonl");
    fs::write(
        &history,
        r#"{"display":"a prompt only the log keeps","timestamp":1772382841000,"sessionId":"gone"}"#
            .to_owned()
            + "\n",
    )
    .unwrap();
    for (route, path, finished_before) in [
        ("claude-code directory", &project, None),
        ("claude-code finished file", &session, finished),
        ("claude-code subagent", &agent, None),
        ("claude-code history", &history, None),
    ] {
        collect(route, &mut |facts| {
            faculties::archive_claude_code::project_path_finished(path, finished_before, |p| {
                *facts += p.fragment.facts().clone();
                Ok(())
            })
            .unwrap();
        });
    }

    // ChatGPT: a titled export with its sidecar file, as a directory and as
    // resident bytes with an attachment map.
    let chatgpt = root.path().join("chatgpt");
    fs::create_dir_all(&chatgpt).unwrap();
    fs::write(chatgpt.join("conversations.json"), CHATGPT_TITLED).unwrap();
    fs::write(chatgpt.join("file-abc-picture.png"), [1, 2, 3, 4]).unwrap();
    collect("chatgpt directory with sidecar", &mut |facts| {
        faculties::archive_chatgpt::project_path(&chatgpt, |p| {
            *facts += p.fragment.facts().clone();
            Ok(())
        })
        .unwrap();
    });
    let sidecar = BTreeMap::from([(
        "file-abc-picture.png".to_owned(),
        Bytes::from(vec![1u8, 2, 3, 4]),
    )]);
    collect("chatgpt bytes with attachments", &mut |facts| {
        faculties::archive_chatgpt::project_bytes(
            "conversations.json",
            Bytes::from(CHATGPT_TITLED),
            &sidecar,
            |p| {
                *facts += p.fragment.facts().clone();
                Ok(())
            },
        )
        .unwrap();
    });

    // Gemini: an activity page with its asset beside it, and as resident
    // bytes with an attachment map.
    let takeout = root.path().join("gemini");
    fs::create_dir_all(&takeout).unwrap();
    let html = gemini(
        "Prompted&nbsp;hello<br>18 Sept 2025, 12:02:52 CET<br><p>world</p><img src=\"answer.png\">",
    );
    let activity = takeout.join("My Activity.html");
    fs::write(&activity, &html).unwrap();
    fs::write(takeout.join("answer.png"), b"png bytes").unwrap();
    collect("gemini file with asset", &mut |facts| {
        faculties::archive_gemini::project_path(&activity, |p| {
            *facts += p.fragment.facts().clone();
            Ok(())
        })
        .unwrap();
    });
    let asset = BTreeMap::from([("answer.png".to_owned(), Bytes::from(b"png bytes".to_vec()))]);
    collect("gemini bytes with attachments", &mut |facts| {
        faculties::archive_gemini::project_bytes(
            "My Activity.html",
            Bytes::from(html.clone()),
            &asset,
            |p| {
                *facts += p.fragment.facts().clone();
                Ok(())
            },
        )
        .unwrap();
    });

    // Codex on the finished route.
    let rollout = root.path().join("rollout.jsonl");
    fs::write(&rollout, CODEX).unwrap();
    collect("codex finished file", &mut |facts| {
        faculties::archive_codex::project_path_finished(&rollout, finished, |p| {
            *facts += p.fragment.facts().clone();
            Ok(())
        })
        .unwrap();
    });
}
