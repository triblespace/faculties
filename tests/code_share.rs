//! `code share`: a reference in, the exact catalogued bytes out to Discord.
//!
//! A fixture repository is ingested into a scratch pile, and every send goes
//! through a fake [`SendTransport`] that records what would have been posted
//! and answers like Discord. Nothing here reaches the network, and the Discord
//! credential is a fixture override, never the Secrets-backed one.

use std::fs;
use std::path::PathBuf;

#[path = "support/code_git.rs"]
mod code_git;

use anybytes::Bytes;
use anyhow::Result;
use clap::Parser;
use faculties::code::operations::{Code, Filter, IngestOptions, Revision};
use faculties::code::share::{Delivery, LineRange, ShareRequest, MESSAGE_LIMIT};
use faculties::code::{cli, mcp};
use faculties::discord::{AllowedMentions, Discord, ReadOptions, SendAttachment, SendTransport};
use faculties::mcp::Faculty;
use faculties::out::{Out, Part};
use faculties::storage::{initialize_signer, Storage};
use serde_json::{json, Value as JsonValue};

const CHANNEL: &str = "100000000000000002";

/// `src/lattice.rs`. Line numbers matter to the assertions below.
const LATTICE: &str = r#"//! A fixture module.
use cubecl::wgpu::WgpuRuntime;

/// One integration step of the force-directed layout, on GPU.
pub fn force_step_kernel(n: u8) -> u8 {
    let doubled = n.wrapping_mul(2);
    doubled.wrapping_add(1)
}

/// A body that holds a fence and a mass mention.
pub fn fenced() -> &'static str {
    "@everyone ``` not a fence"
}
"#;

/// Two different declarations of one name, in two files.
const LEFT: &str = "pub fn twin() -> u8 {\n    1\n}\n";
const RIGHT: &str = "pub fn twin() -> u8 {\n    2\n}\n";

fn long_source() -> String {
    let mut source = String::from("pub fn long_kernel() -> u64 {\n    let mut total = 0u64;\n");
    for step in 0..120 {
        source.push_str(&format!("    total = total.wrapping_add({step});\n"));
    }
    source.push_str("    total\n}\n");
    source
}

/// Lines `start..=end` of `text`, independently of the code under test.
fn lines_of(text: &str, start: usize, end: usize) -> String {
    text.split_inclusive('\n')
        .skip(start - 1)
        .take(end - start + 1)
        .collect()
}

struct Fixture {
    _directory: tempfile::TempDir,
    pile: PathBuf,
    key: PathBuf,
    commit: String,
}

impl Fixture {
    fn new() -> Self {
        for variable in [
            "DRIVE_ENDPOINT",
            "DRIVE_KEY",
            "TRIBLESPACE_COLLECTION_CODE",
            "TRIBLESPACE_COLLECTION_DISCORD",
            "TRIBLESPACE_KEY",
            "DISCORD_TOKEN",
            "PILE",
        ] {
            std::env::remove_var(variable);
        }
        std::env::set_var("PERSONA", "ambient-not-a-target");

        let directory = tempfile::tempdir().unwrap();
        let repo = directory.path().join("fixture-repo");
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::write(repo.join("src/lattice.rs"), LATTICE).unwrap();
        fs::write(repo.join("src/left.rs"), LEFT).unwrap();
        fs::write(repo.join("src/right.rs"), RIGHT).unwrap();
        fs::write(repo.join("src/long.rs"), long_source()).unwrap();
        code_git::initialize(&repo);
        let commit = git2::Repository::open(&repo)
            .unwrap()
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .id()
            .to_string();

        let pile = directory.path().join("share.pile");
        let key = directory.path().join("share.key");
        fs::File::create(&pile).unwrap();
        initialize_signer(&pile, Some(&key)).unwrap();
        let fixture = Self {
            _directory: directory,
            pile,
            key,
            commit,
        };
        fixture
            .code()
            .ingest(&[repo.clone()], &IngestOptions::default())
            .unwrap();
        // The working tree moves on after the scan; a share reads the pile's
        // bytes and must never see this.
        fs::write(
            repo.join("src/lattice.rs"),
            "pub fn force_step_kernel() {}\n",
        )
        .unwrap();
        fixture
    }

    fn storage(&self) -> Storage {
        Storage::new(self.pile.clone(), Some(self.key.clone()))
    }

    fn code(&self) -> Code {
        Code::with_storage(self.storage())
    }

    fn discord(&self) -> Discord {
        Discord::with_storage(self.storage()).with_token("fixture-token".to_owned())
    }
}

#[derive(Debug)]
struct Posted {
    channel: String,
    text: String,
    attachments: Vec<SendAttachment>,
    mentions: Option<AllowedMentions>,
    reply_to: Option<String>,
}

/// Records each post and answers as Discord would, CDN urls included.
#[derive(Default)]
struct FakeDiscord {
    posts: Vec<Posted>,
    downloads: Vec<String>,
}

fn cdn(message: usize, filename: &str) -> String {
    format!("https://cdn.discordapp.com/attachments/{CHANNEL}/{message}/{filename}")
}

impl SendTransport for FakeDiscord {
    fn post(
        &mut self,
        token: &str,
        channel_id: &str,
        text: &str,
        attachments: &[SendAttachment],
        mentions: Option<&AllowedMentions>,
        reply_to: Option<&str>,
    ) -> Result<JsonValue> {
        assert_eq!(token, "fixture-token");
        self.posts.push(Posted {
            channel: channel_id.to_owned(),
            text: text.to_owned(),
            attachments: attachments.to_vec(),
            mentions: mentions.cloned(),
            reply_to: reply_to.map(str::to_owned),
        });
        let index = self.posts.len();
        let returned: Vec<JsonValue> = attachments
            .iter()
            .enumerate()
            .map(|(offset, attachment)| {
                json!({
                    "id": format!("2000000000000000{index:02}{offset}"),
                    "filename": attachment.filename,
                    "content_type": attachment.media_type,
                    "size": attachment.bytes.len(),
                    "url": cdn(index, &attachment.filename),
                })
            })
            .collect();
        Ok(json!({
            "id": format!("1000000000000001{index:02}"),
            "channel_id": channel_id,
            "content": text,
            "author": {"id": "100000000000000010", "username": "fixture-bot", "bot": true},
            "timestamp": "2026-11-15T08:00:00Z",
            "edited_timestamp": null,
            "attachments": returned,
            "referenced_message": null,
        }))
    }

    fn attachment(&mut self, url: &str, _limit: u64) -> Result<Vec<u8>> {
        self.downloads.push(url.to_owned());
        for (index, posted) in self.posts.iter().enumerate() {
            for attachment in &posted.attachments {
                if cdn(index + 1, &attachment.filename) == url {
                    return Ok(attachment.bytes.clone());
                }
            }
        }
        anyhow::bail!("fixture CDN has no {url}")
    }
}

fn request(selector: &str) -> ShareRequest {
    ShareRequest {
        selector: selector.to_owned(),
        ..ShareRequest::default()
    }
}

fn share(
    fixture: &Fixture,
    fake: &mut FakeDiscord,
    request: &ShareRequest,
    reply_to: Option<&str>,
) -> Result<faculties::code::share::ShareReceipt> {
    fixture
        .code()
        .share_through(request, CHANNEL, reply_to, &fixture.discord(), fake)
}

fn render_to_string(render: impl FnOnce(&mut Out<'_>) -> Result<()>) -> String {
    let mut text = String::new();
    {
        let mut emit = |part: Part| {
            if let Part::Text { text: part } = part {
                text.push_str(&part);
            }
            Ok(())
        };
        let mut out = Out::new(&mut emit);
        render(&mut out).unwrap();
    }
    text
}

#[test]
fn a_short_declaration_is_posted_as_its_exact_bytes_in_a_rust_block() {
    let fixture = Fixture::new();
    let mut fake = FakeDiscord::default();
    let receipt = share(&fixture, &mut fake, &request("force_step_kernel"), None).unwrap();

    assert_eq!(fake.posts.len(), 1);
    let posted = &fake.posts[0];
    assert_eq!(posted.channel, CHANNEL);
    assert!(posted.attachments.is_empty());
    let span = receipt.plan.lines;
    let expected = lines_of(LATTICE, span.start as usize, span.end as usize);
    assert!(expected.contains("pub fn force_step_kernel"), "{expected}");
    assert_eq!(receipt.plan.bytes, expected.as_bytes());
    assert_eq!(receipt.plan.commit, fixture.commit);
    assert_eq!(receipt.plan.delivery, Delivery::Block);
    let header = format!(
        "`fixture-repo/src/lattice.rs` lines {}-{} at fixture-repo@{} (fn force_step_kernel)",
        span.start,
        span.end,
        &fixture.commit[..12]
    );
    assert_eq!(
        posted.text,
        format!(
            "{header}\n```rust\n{}\n```",
            expected.strip_suffix('\n').unwrap()
        )
    );
    // Nobody is pinged by shared source, whatever it spells.
    assert_eq!(posted.mentions, Some(AllowedMentions::nobody()));

    // The post is stored like any `discord send`.
    let history = fixture
        .discord()
        .read(ReadOptions {
            channel_id: Some(CHANNEL.to_owned()),
            ..ReadOptions::default()
        })
        .unwrap();
    assert_eq!(history.messages.len(), 1);
    assert_eq!(history.messages[0].content, posted.text);
    assert_eq!(receipt.sent.message_id, "100000000000000101");
}

#[test]
fn an_id_prefix_selects_the_same_bytes_as_the_name() {
    let fixture = Fixture::new();
    let by_name = fixture
        .code()
        .share_plan(&request("force_step_kernel"))
        .unwrap();
    let prefix = format!("{:x}", by_name.item);
    let by_id = fixture.code().share_plan(&request(&prefix[..10])).unwrap();
    assert_eq!(by_id.item, by_name.item);
    assert_eq!(by_id.bytes, by_name.bytes);
    assert_eq!(by_id.text, by_name.text);
}

#[test]
fn a_long_declaration_goes_as_an_exact_attachment_and_is_stored_with_it() {
    let fixture = Fixture::new();
    let mut fake = FakeDiscord::default();
    let receipt = share(&fixture, &mut fake, &request("long_kernel"), None).unwrap();
    let source = long_source();
    assert!(source.chars().count() > MESSAGE_LIMIT);

    let posted = &fake.posts[0];
    assert_eq!(posted.attachments.len(), 1);
    let attachment = &posted.attachments[0];
    let last = source.lines().count();
    assert_eq!(attachment.filename, format!("long.L1-{last}.rs"));
    assert_eq!(attachment.bytes, source.as_bytes(), "never truncated");
    assert!(posted.text.chars().count() <= MESSAGE_LIMIT);
    assert!(
        posted.text.starts_with(&format!(
            "`fixture-repo/src/long.rs` lines 1-{last} at fixture-repo@{}",
            &fixture.commit[..12]
        )),
        "{}",
        posted.text
    );
    assert!(
        posted.text.contains("attached as long.L1-"),
        "{}",
        posted.text
    );
    assert!(matches!(receipt.plan.delivery, Delivery::Attachment { .. }));

    // The returned attachment was downloaded and stored with the observation.
    assert_eq!(fake.downloads.len(), 1);
    let (_, stored) = fixture
        .discord()
        .attachment_bytes("2000000000000000010", &attachment.filename)
        .unwrap()
        .expect("attachment bytes stored");
    assert_eq!(stored.as_ref(), source.as_bytes());
}

#[test]
fn a_declaration_holding_a_fence_is_attached_rather_than_mangled() {
    let fixture = Fixture::new();
    let plan = fixture.code().share_plan(&request("fenced")).unwrap();
    let Delivery::Attachment { reason, .. } = &plan.delivery else {
        panic!("expected an attachment, got {:?}", plan.delivery);
    };
    assert!(reason.contains("code fence"), "{reason}");
    assert_eq!(
        plan.attachments[0].bytes,
        lines_of(LATTICE, plan.lines.start as usize, plan.lines.end as usize).as_bytes()
    );
}

#[test]
fn a_line_range_slices_inside_the_declaration_and_refuses_outside_it() {
    let fixture = Fixture::new();
    let whole = fixture
        .code()
        .share_plan(&request("force_step_kernel"))
        .unwrap();
    let inner = LineRange {
        start: whole.declaration.start + 1,
        end: whole.declaration.end - 1,
    };
    let mut fake = FakeDiscord::default();
    let receipt = share(
        &fixture,
        &mut fake,
        &ShareRequest {
            lines: Some(inner),
            ..request("force_step_kernel")
        },
        None,
    )
    .unwrap();
    let expected = lines_of(LATTICE, inner.start as usize, inner.end as usize);
    assert_eq!(receipt.plan.bytes, expected.as_bytes());
    assert!(
        fake.posts[0].text.starts_with(&format!(
            "`fixture-repo/src/lattice.rs` lines {}-{} ",
            inner.start, inner.end
        )),
        "{}",
        fake.posts[0].text
    );

    for outside in [
        LineRange {
            start: whole.declaration.start - 1,
            end: whole.declaration.end,
        },
        LineRange {
            start: whole.declaration.start,
            end: whole.declaration.end + 1,
        },
        LineRange {
            start: 900,
            end: 901,
        },
    ] {
        let mut fake = FakeDiscord::default();
        let error = share(
            &fixture,
            &mut fake,
            &ShareRequest {
                lines: Some(outside),
                ..request("force_step_kernel")
            },
            None,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("outside"), "{error:#}");
        assert!(fake.posts.is_empty(), "a refused range must send nothing");
    }
}

#[test]
fn an_unknown_or_ambiguous_selector_sends_nothing_and_says_why() {
    let fixture = Fixture::new();

    let mut fake = FakeDiscord::default();
    let error = share(&fixture, &mut fake, &request("LearnerBuilder"), None).unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("no catalogued declaration"), "{message}");
    assert!(message.contains("nothing was sent"), "{message}");
    assert!(fake.posts.is_empty());

    let error = share(&fixture, &mut fake, &request("twin"), None).unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("matches 2 placements"), "{message}");
    assert!(
        message.contains("fixture-repo/src/left.rs:1-3"),
        "{message}"
    );
    assert!(
        message.contains("fixture-repo/src/right.rs:1-3"),
        "{message}"
    );
    assert!(fake.posts.is_empty());

    // A path tells them apart.
    let receipt = share(
        &fixture,
        &mut fake,
        &ShareRequest {
            path: Some("src/right.rs".to_owned()),
            ..request("twin")
        },
        None,
    )
    .unwrap();
    assert_eq!(receipt.plan.bytes, RIGHT.as_bytes());
    assert_eq!(fake.posts.len(), 1);

    // A revision nothing was scanned at matches nothing.
    let mut fake = FakeDiscord::default();
    let error = share(
        &fixture,
        &mut fake,
        &ShareRequest {
            filter: Filter {
                revision: Revision::Selector("fixture-repo@ffffffff".to_owned()),
                ..Filter::default()
            },
            ..request("force_step_kernel")
        },
        None,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("no Code scan"), "{error:#}");
    assert!(fake.posts.is_empty());
}

#[test]
fn malformed_ids_and_an_unstored_reply_target_send_nothing() {
    let fixture = Fixture::new();
    let mut fake = FakeDiscord::default();
    let code = fixture.code();
    let discord = fixture.discord();
    assert!(code
        .share_through(
            &request("force_step_kernel"),
            "general",
            None,
            &discord,
            &mut fake
        )
        .is_err());
    assert!(code
        .share_through(
            &request("force_step_kernel"),
            CHANNEL,
            Some("not-an-id"),
            &discord,
            &mut fake
        )
        .is_err());
    let error = share(
        &fixture,
        &mut fake,
        &request("force_step_kernel"),
        Some("100000000000000999"),
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("is not stored in channel"),
        "{error:#}"
    );
    assert!(fake.posts.is_empty());

    // A stored target is replied to through the same path.
    let first = share(&fixture, &mut fake, &request("force_step_kernel"), None).unwrap();
    share(
        &fixture,
        &mut fake,
        &request("fenced"),
        Some(&first.sent.message_id),
    )
    .unwrap();
    assert_eq!(fake.posts.len(), 2);
    assert_eq!(
        fake.posts[1].reply_to.as_deref(),
        Some(first.sent.message_id.as_str())
    );
}

#[test]
fn the_cli_and_mcp_dry_runs_name_the_reference_and_never_the_bytes() {
    let fixture = Fixture::new();
    let cli_text = render_to_string(|out| {
        cli::execute(
            cli::Cli::try_parse_from([
                "code".into(),
                "--pile".into(),
                fixture.pile.as_os_str().to_owned(),
                "--key".into(),
                fixture.key.as_os_str().to_owned(),
                "share".into(),
                "force_step_kernel".into(),
                "--channel".into(),
                CHANNEL.into(),
                "--lines".into(),
                "6-7".into(),
                "--dry-run".into(),
            ])
            .unwrap(),
            out,
        )
    });
    let adapter = mcp::Code::new(fixture.pile.clone(), Some(fixture.key.clone()));
    let arguments = serde_json::to_vec(&json!({
        "selector": "force_step_kernel", "channel_id": CHANNEL, "lines": "6-7", "dry_run": true,
    }))
    .unwrap();
    let mcp_text =
        render_to_string(|out| adapter.call("code_share", Bytes::from_source(arguments), out));
    assert_eq!(cli_text, mcp_text);
    assert!(
        cli_text.contains("fixture-repo/src/lattice.rs lines 6-7"),
        "{cli_text}"
    );
    assert!(cli_text.contains(&fixture.commit), "{cli_text}");
    assert!(cli_text.contains("nothing was sent"), "{cli_text}");
    assert!(
        !cli_text.contains("wrapping_mul"),
        "a dry run must not print the bytes"
    );
}

#[test]
fn the_mcp_tool_refuses_source_text_as_an_argument() {
    let fixture = Fixture::new();
    let adapter = mcp::Code::new(fixture.pile.clone(), Some(fixture.key.clone()));
    let tool = adapter
        .tools()
        .iter()
        .find(|tool| tool.name == "code_share")
        .expect("code_share is registered");
    for forbidden in ["\"text\"", "\"source\"", "\"content\"", "\"attachments\""] {
        assert!(!tool.input_schema.contains(forbidden), "{forbidden}");
    }
    let mut sink = |_: Part| Ok(());
    let mut out = Out::new(&mut sink);
    let error = adapter
        .call(
            "code_share",
            Bytes::from_source(
                serde_json::to_vec(&json!({
                    "selector": "force_step_kernel", "channel_id": CHANNEL,
                    "text": "fn forged() {}", "dry_run": true,
                }))
                .unwrap(),
            ),
            &mut out,
        )
        .unwrap_err();
    assert!(format!("{error:#}").contains("text"), "{error:#}");
}
