//! Discord frontends over temporary resident observations; no real bot or network requests.
use anybytes::Bytes;
use anyhow::{bail, Result};
use faculties::discord::{self, Discord, PullOptions, ReadOptions};
use faculties::mcp::{Faculty, InvalidArguments, Server};
use faculties::out::{Out, Part};
use faculties::schemas::{archive::archive, discord::discord as schema};
use faculties::storage::{initialize_signer, publish_fragment};
use hifitime::Epoch;
use serde_json::{json, Value};
use std::{fs, path::PathBuf, process::Command};
use triblespace::core::metadata;
use triblespace::prelude::inlineencodings::NsTAIInterval;
use triblespace::prelude::*;

const CHANNEL: &str = "100000000000000002";
const MESSAGE: &str = "100000000000000003";

struct Fixture {
    _directory: tempfile::TempDir,
    pile: PathBuf,
    key: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let pile = directory.path().join("discord.pile");
        let key = directory.path().join("discord.key");
        fs::File::create(&pile).unwrap();
        initialize_signer(&pile, Some(&key)).unwrap();
        Self {
            _directory: directory,
            pile,
            key,
        }
    }
    fn operations(&self) -> Discord {
        Discord::new(self.pile.clone(), Some(self.key.clone()))
    }
    fn mcp(&self) -> discord::mcp::Discord {
        discord::mcp::Discord::new(self.pile.clone(), Some(self.key.clone()))
    }
    fn publish(&self, fragment: Fragment) {
        publish_fragment(
            &self.pile,
            Some(&self.key),
            faculties::schemas::discord::DEFAULT_SCOPE_ID,
            fragment,
        )
        .unwrap();
    }
}
fn at(seconds: f64) -> Inline<NsTAIInterval> {
    let epoch = Epoch::from_unix_seconds(seconds);
    (epoch, epoch).try_to_inline().unwrap()
}
fn observation(message: &str, content: &str, created: f64, edited: Option<f64>) -> Fragment {
    let mut fragment = discord::message_anchor_fragment(message).unwrap();
    let anchor = fragment.root().unwrap();
    let channel = discord::channel_fragment(CHANNEL).unwrap();
    let channel_id = channel.root().unwrap();
    fragment += channel;
    let author = discord::user_fragment("100000000000000004").unwrap();
    let author_id = author.root().unwrap();
    fragment += author;
    fragment += entity! { _ @ metadata::tag: schema::kind_user_profile, schema::user: author_id, archive::author_name: "Ada".to_owned() };
    fragment += entity! { _ @
        metadata::tag: archive::kind_message, schema::message: anchor, schema::channel: channel_id,
        archive::author: author_id, archive::content: content.to_owned(), metadata::created_at: at(created),
        archive::edited_at?: edited.map(at),
    };
    fragment
}
fn collect(execute: impl FnOnce(&mut Out<'_>) -> Result<()>) -> Result<String> {
    let mut text = String::new();
    execute(&mut Out::new(&mut |part| {
        match part {
            Part::Text { text: part } => text.push_str(&part),
            _ => panic!("unexpected modality"),
        }
        Ok(())
    }))?;
    Ok(text)
}
fn call(faculty: &discord::mcp::Discord, tool: &str, args: Value) -> Result<String> {
    collect(|out| faculty.call(tool, Bytes::from(serde_json::to_vec(&args).unwrap()), out))
}

#[test]
fn cli_and_mcp_bind_the_same_exact_secret_without_returning_plaintext() {
    let fixture = Fixture::new();
    let plaintext = b"synthetic-frontend-credential";
    let secret = faculties::secrets::Secrets::new(fixture.pile.clone(), Some(fixture.key.clone()))
        .add("bot", plaintext)
        .unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_discord"));
    clean_child(&mut command);
    let output = command
        .arg("--pile")
        .arg(&fixture.pile)
        .arg("--key")
        .arg(&fixture.key)
        .args(["auth", "set", &format!("{secret:X}")])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let status = call(&fixture.mcp(), "discord_auth_status", json!({})).unwrap();
    assert!(status.contains(&format!("{secret:X}")));
    assert!(status.contains("settled"));
    let response = call(
        &fixture.mcp(),
        "discord_auth_set",
        json!({"secret_version": format!("{secret:X}")}),
    )
    .unwrap();
    assert!(response.contains(&format!("{secret:X}")));
    let text = std::str::from_utf8(plaintext).unwrap();
    for rendered in [&status, &response] {
        assert!(!rendered.contains(text));
    }
    assert!(!String::from_utf8_lossy(&output.stdout).contains(text));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(text));
    assert_eq!(
        fixture
            .operations()
            .auth_status()
            .unwrap()
            .selected()
            .unwrap(),
        secret
    );
}
fn clean_child(command: &mut Command) {
    for (name, _) in std::env::vars_os() {
        let text = name.to_string_lossy();
        if text.starts_with("TRIBLESPACE_")
            || text.starts_with("DRIVE_")
            || matches!(text.as_ref(), "PILE" | "PERSONA" | "DISCORD_TOKEN")
        {
            command.env_remove(name);
        }
    }
}

#[test]
fn native_and_mcp_resident_reads_need_no_bot_token() {
    let fixture = Fixture::new();
    fixture.publish(observation(MESSAGE, "@- literal message", 10.0, None));
    let history = fixture.operations().read(ReadOptions::default()).unwrap();
    assert_eq!(history.messages.len(), 1);
    assert_eq!(history.messages[0].content, "@- literal message");
    assert_eq!(
        history.messages[0].anchor,
        discord::message_anchor_fragment(MESSAGE)
            .unwrap()
            .root()
            .unwrap()
    );
    assert_eq!(
        collect(|out| discord::render::history(&history, out)).unwrap(),
        call(&fixture.mcp(), "discord_read", json!({})).unwrap()
    );

    // The CLI `read` is resident-only since e9e1addd, like `discord_read`:
    // no token, and the same rendering as MCP.
    let run = |args: &[&str]| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_discord"));
        clean_child(&mut command);
        command
            .arg("--pile")
            .arg(&fixture.pile)
            .arg("--key")
            .arg(&fixture.key)
            .args(args)
            .output()
            .unwrap()
    };
    let output = run(&["read", CHANNEL]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let channel = fixture
        .operations()
        .read(ReadOptions {
            channel_id: Some(CHANNEL.into()),
            ..ReadOptions::default()
        })
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        collect(|out| discord::render::history(&channel, out)).unwrap()
    );

    // Fetching from Discord is `pull`, and that still needs the token.
    let output = run(&["pull", CHANNEL]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("Discord authentication is not configured"));
}

#[test]
fn owned_history_keeps_old_snapshot_and_exposes_divergent_edits() {
    let fixture = Fixture::new();
    fixture.publish(observation(MESSAGE, "original", 10.0, None));
    let old = fixture.operations().read(ReadOptions::default()).unwrap();
    fixture.publish(observation(MESSAGE, "edited", 10.0, Some(20.0)));
    fixture.publish(observation(MESSAGE, "divergent", 10.0, Some(20.0)));
    assert_eq!(old.messages[0].content, "original");
    let current = fixture.operations().read(ReadOptions::default()).unwrap();
    assert_eq!(current.messages.len(), 2);
    assert!(current
        .messages
        .iter()
        .all(|value| value.variant_count == 2));
    let shown = call(
        &fixture.mcp(),
        "discord_read",
        json!({"channel_id":CHANNEL,"limit":0}),
    )
    .unwrap();
    assert!(shown.contains("[DIVERGENT 1/2]"));
    assert!(shown.contains("[DIVERGENT 2/2]"));
    assert!(shown.contains("edited"));
    assert!(shown.contains("divergent"));
    assert!(!shown.contains("original"));
}

#[test]
fn selected_history_does_not_acquire_unselected_cold_message_body() {
    let fixture = Fixture::new();
    fixture.publish(
        observation(MESSAGE, "cold text", 10.0, None)
            .into_facts()
            .into(),
    );
    fixture.publish(observation("100000000000000005", "warm text", 20.0, None));
    assert!(call(&fixture.mcp(), "discord_read", json!({"limit":1}))
        .unwrap()
        .contains("warm text"));
    let mut emitted = 0;
    let error = fixture
        .mcp()
        .call(
            "discord_read",
            Bytes::from(br#"{"limit":0}"#.to_vec()),
            &mut Out::new(&mut |_| {
                emitted += 1;
                Ok(())
            }),
        )
        .unwrap_err();
    assert!(format!("{error:#}").contains("Discord message content"));
    assert_eq!(emitted, 0);
}

#[test]
fn decoder_rejects_bad_ids_numeric_types_duplicates_and_host_fields_without_storage() {
    let directory = tempfile::tempdir().unwrap();
    let pile = directory.path().join("absent.pile");
    let faculty = discord::mcp::Discord::new(pile.clone(), None);
    Server::new(&[&faculty]).unwrap();
    assert_eq!(
        faculty
            .tools()
            .iter()
            .map(|tool| tool.name)
            .collect::<Vec<_>>(),
        [
            "discord_auth_status",
            "discord_auth_set",
            "discord_read",
            "discord_pull",
            "discord_send",
            "discord_channels_list"
        ]
    );
    for (tool, args) in [
        ("discord_read", r#"{"limit":-1}"#),
        ("discord_read", r#"{"limit":"2"}"#),
        ("discord_read", r#"{"limit":1.5}"#),
        ("discord_read", r#"{"channel_id":"01"}"#),
        ("discord_read", r#"{"since":"not a timestamp"}"#),
        ("discord_pull", r#"{"fetch_limit":0}"#),
        ("discord_pull", r#"{"reconcile_limit":101}"#),
        ("discord_pull", r#"{"token":"do-not-output"}"#),
        (
            "discord_send",
            r#"{"channel_id":"1","text":"a","text":"b"}"#,
        ),
        ("discord_send", r#"{"channel_id":1,"text":"a"}"#),
        ("discord_send", r#"{"channel_id":"1","text":" "}"#),
        (
            "discord_send",
            r#"{"channel_id":"1","text":"a","pile":"/elsewhere"}"#,
        ),
        (
            "discord_send",
            r#"{"channel_id":"1","text":"a","reply_to":"01"}"#,
        ),
        (
            "discord_send",
            r#"{"channel_id":"1","text":"a","reply_to":5}"#,
        ),
        (
            "discord_channels_list",
            r#"{"guild":"18446744073709551616"}"#,
        ),
        ("discord_channels_list", r#"{"guild":"1","guild":"2"}"#),
    ] {
        let error = collect(|out| faculty.call(tool, Bytes::from(args.as_bytes().to_vec()), out))
            .unwrap_err();
        assert!(error.is::<InvalidArguments>(), "{tool}: {error:#}");
    }
    assert!(!pile.exists());
}

#[test]
fn missing_bot_config_fails_before_network_and_debug_never_discloses_it() {
    let fixture = Fixture::new();
    let faculty = fixture.mcp();
    for (tool, args) in [
        (
            "discord_send",
            json!({"channel_id":"1","text":"@/not-a-host-file"}),
        ),
        ("discord_pull", json!({"channel_id":"1"})),
        ("discord_channels_list", json!({})),
    ] {
        let error = call(&faculty, tool, args).unwrap_err();
        assert!(
            format!("{error:#}").contains("Discord authentication is not configured"),
            "{tool}: {error:#}"
        );
    }
    let token = "test-configured-bot-token";
    assert!(!format!("{faculty:?}").contains(token));
    let native = fixture.operations().with_token(token.to_owned());
    assert!(!format!("{native:?}").contains(token));
    assert!(native
        .pull(PullOptions {
            fetch_limit: 0,
            ..Default::default()
        })
        .is_err());
}

#[test]
fn mcp_does_not_inherit_an_ambient_bot_token_and_cli_help_redacts_it() {
    if std::env::var_os("FACULTIES_DISCORD_AMBIENT_CHILD").is_none() {
        let mut command = Command::new(std::env::current_exe().unwrap());
        clean_child(&mut command);
        let output = command
            .args([
                "--exact",
                "mcp_does_not_inherit_an_ambient_bot_token_and_cli_help_redacts_it",
                "--nocapture",
            ])
            .env("FACULTIES_DISCORD_AMBIENT_CHILD", "1")
            .env("DISCORD_TOKEN", "test-only-secret-environment-token")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        return;
    }
    let fixture = Fixture::new();
    let faculty = fixture.mcp();
    assert!(call(&faculty, "discord_channels_list", json!({}))
        .unwrap_err()
        .to_string()
        .contains("Discord authentication is not configured"));
    let output = Command::new(env!("CARGO_BIN_EXE_discord"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("test-only-secret-environment-token"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("test-only-secret-environment-token"));
}

#[test]
fn output_failure_stops_history_delivery_without_mutating_resident_evidence() {
    let fixture = Fixture::new();
    fixture.publish(observation(MESSAGE, "unchanged", 10.0, None));
    fixture.operations().read(ReadOptions::default()).unwrap();
    let before = fs::read(&fixture.pile).unwrap();
    let error = fixture
        .mcp()
        .call(
            "discord_read",
            Bytes::from(br#"{}"#.to_vec()),
            &mut Out::new(&mut |_| bail!("fixture delivery failure")),
        )
        .unwrap_err();
    assert!(error.to_string().contains("fixture delivery failure"));
    assert_eq!(fs::read(&fixture.pile).unwrap(), before);
}

/// A message line carries its Discord id, and a reply says what it replies
/// to: the target's author and first words when it is stored, its id when it
/// is not. CLI and MCP render the same lines.
#[test]
fn read_shows_each_message_id_and_what_a_reply_replies_to() {
    let fixture = Fixture::new();
    let target = "100000000000000005";
    let gone = "100000000000000009";
    fixture.publish(observation(target, "is anyone there?", 10.0, None));
    for (id, replies_to, text) in [
        ("100000000000000006", target, "yes, here"),
        ("100000000000000007", gone, "to a message never stored"),
    ] {
        let mut fragment = observation(id, text, 20.0, None);
        let reply = discord::message_anchor_fragment(replies_to).unwrap();
        let reply_id = reply.root().unwrap();
        let observation = find!(
            observation: Id,
            pattern!(fragment.facts(), [{ ?observation @ metadata::tag: archive::kind_message }])
        )
        .next()
        .unwrap();
        fragment += reply;
        fragment += entity! { ExclusiveId::force_ref(&observation) @ archive::reply_to: reply_id };
        fixture.publish(fragment);
    }
    // Read one channel, so no line carries the channel label.
    let shown = call(
        &fixture.mcp(),
        "discord_read",
        json!({"channel_id": CHANNEL, "limit": 0}),
    )
    .unwrap();
    assert!(
        shown.contains(&format!("] id={target} Ada: is anyone there?")),
        "{shown}"
    );
    assert!(
        shown.contains(&format!(
            "] id=100000000000000006 Ada (reply to id={target} by Ada: \"is anyone there?\"): yes, here"
        )),
        "{shown}"
    );
    assert!(
        shown.contains(&format!(
            "] id=100000000000000007 Ada (reply to id={gone}, not stored here): to a message never stored"
        )),
        "{shown}"
    );
    let mut command = Command::new(env!("CARGO_BIN_EXE_discord"));
    clean_child(&mut command);
    let output = command
        .arg("--pile")
        .arg(&fixture.pile)
        .arg("--key")
        .arg(&fixture.key)
        .args(["read", CHANNEL, "--limit", "0"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), shown);
}

/// A reply to a message the channel does not hold is refused by CLI and MCP
/// alike before anything is posted: the refusal names the target, and
/// nothing reaches Discord (a post would fail as an unknown outcome, never
/// as this).
#[test]
fn cli_and_mcp_refuse_a_reply_to_a_message_not_stored_in_the_channel() {
    let fixture = Fixture::new();
    let secret = faculties::secrets::Secrets::new(fixture.pile.clone(), Some(fixture.key.clone()))
        .add("bot", b"synthetic-reply-credential")
        .unwrap();
    fixture.operations().auth_set(secret).unwrap();
    // Stored, but in another channel.
    fixture.publish(observation(MESSAGE, "elsewhere", 10.0, None));
    for target in [MESSAGE, "100000000000000099"] {
        let error = call(
            &fixture.mcp(),
            "discord_send",
            json!({"channel_id": "100000000000000020", "text": "hi", "reply_to": target}),
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains(&format!(
                "message {target} is not stored in channel 100000000000000020, so nothing was sent"
            )),
            "{error:#}"
        );
        let mut command = Command::new(env!("CARGO_BIN_EXE_discord"));
        clean_child(&mut command);
        let output = command
            .arg("--pile")
            .arg(&fixture.pile)
            .arg("--key")
            .arg(&fixture.key)
            .args(["send", "100000000000000020", "hi", "--reply-to", target])
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("so nothing was sent"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
