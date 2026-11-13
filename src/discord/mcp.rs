//! Explicit finite Discord MCP operations; authentication resolves through Secrets.
use super::{operations::*, render};
use crate::mcp::{decode_arguments, invalid_arguments, Faculty, Tool};
use crate::out::Out;
use anybytes::Bytes;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::PathBuf;

const TOOLS: &[Tool] = &[
    Tool { name: "discord_auth_status", description: "Show the workspace's Discord authentication revision and exact encrypted Secrets references. Never opens or returns credential plaintext.",
        input_schema: r#"{"type":"object","properties":{},"additionalProperties":false}"# },
    Tool { name: "discord_auth_set", description: "Bind an existing exact encrypted Secrets version as this workspace's Discord bot credential, reconciling observed auth revisions. Requires access to that version and Discord collection WRITE; never imports plaintext or grants authority.",
        input_schema: r#"{"type":"object","properties":{"secret_version":{"type":"string","description":"Exact 32-character immutable Secrets version id; never a token or secret name."}},"required":["secret_version"],"additionalProperties":false}"# },
    Tool { name: "discord_read", description: "Read resident Discord observations, including divergent latest versions. Each message line carries its Discord message id (id=...), which discord_send takes as reply_to; a reply says which message it replies to: that message's id, and its author and first words when it is stored. No token or network access required; call discord_pull explicitly for fresh data.",
        input_schema: r#"{"type":"object","properties":{"channel_id":{"type":"string"},"since":{"type":"string","description":"RFC3339 timestamp."},"limit":{"type":"integer","minimum":0,"default":20,"description":"Newest N messages; 0 means unlimited."},"descending":{"type":"boolean","default":false}},"additionalProperties":false}"# },
    Tool { name: "discord_pull", description: "Pull a complete forward interval and bounded recent edits into the archive using the workspace's Secrets-backed bot credential. Omit channel_id to visit all visible text-capable channels; partial channel failures are reported explicitly.",
        input_schema: r#"{"type":"object","properties":{"channel_id":{"type":"string"},"fetch_limit":{"type":"integer","minimum":1,"maximum":100,"default":100},"reconcile_limit":{"type":"integer","minimum":1,"maximum":100,"default":50}},"additionalProperties":false}"# },
    Tool { name: "discord_send", description: "Send literal text and/or attachments as the workspace's Secrets-backed Discord bot after collection WRITE admission, then store the returned observation and attachment bytes. With reply_to, the message is posted as a Discord reply to that stored message. At most 10 files and 20 MiB decoded bytes total; transport request limits may be smaller. An uncertain send or post-send retention failure must not be retried automatically.",
        input_schema: r#"{"type":"object","properties":{"channel_id":{"type":"string"},"text":{"type":"string","maxLength":2000,"description":"Optional literal message body. @file and @- never expand to host input."},"reply_to":{"type":"string","pattern":"^[1-9][0-9]{0,19}$","description":"Optional Discord message id to reply to, as discord_read shows it (id=...). It must be a message stored in this channel; otherwise nothing is sent."},"attachments":{"type":"array","maxItems":10,"items":{"type":"object","properties":{"filename":{"type":"string","minLength":1,"maxLength":255,"description":"Leaf filename, without path separators or control characters."},"media_type":{"type":"string","maxLength":255,"description":"MIME type, e.g. image/png or application/pdf."},"file_id":{"type":"string","pattern":"^[0-9a-fA-F]{32}$","description":"Exact 32-hex Files entity ID or single-file import ID in this workspace, resolved as files_get. Use the Entity or Import ID from files_show or files_add, not a content Hash. A folder import is refused."},"path":{"type":"string","description":"Absolute path of the file on the MCP server's host, read with the server process's permissions, which are a capability, not authority: pass only paths the user or task authorizes, and workspace selection authorizes none."}},"required":["filename","media_type"],"oneOf":[{"required":["file_id"],"not":{"required":["path"]}},{"required":["path"],"not":{"required":["file_id"]}}],"additionalProperties":false}}},"required":["channel_id"],"additionalProperties":false}"# },
    Tool { name: "discord_channels_list", description: "List the guilds and channels visible to the workspace's Secrets-backed Discord bot through a finite REST query.",
        input_schema: r#"{"type":"object","properties":{"guild":{"type":"string","description":"Optional exact canonical Discord guild snowflake."}},"additionalProperties":false}"# },
];
fn limit() -> usize {
    20
}
fn fetch_limit() -> u32 {
    100
}
fn reconcile_limit() -> u32 {
    50
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Read {
    channel_id: Option<String>,
    since: Option<String>,
    #[serde(default = "limit")]
    limit: usize,
    #[serde(default)]
    descending: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Pull {
    channel_id: Option<String>,
    #[serde(default = "fetch_limit")]
    fetch_limit: u32,
    #[serde(default = "reconcile_limit")]
    reconcile_limit: u32,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Send {
    channel_id: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    attachments: Vec<Attachment>,
    reply_to: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Attachment {
    filename: String,
    media_type: String,
    file_id: Option<String>,
    path: Option<String>,
}
impl Send {
    fn decode_attachments(
        &self,
        files: &crate::files::operations::Files,
    ) -> Result<Vec<SendAttachment>> {
        if self.attachments.len() > MAX_SEND_ATTACHMENTS {
            bail!("at most {MAX_SEND_ATTACHMENTS} attachments per message");
        }
        let mut attachments: Vec<_> = self
            .attachments
            .iter()
            .map(|attachment| SendAttachment {
                filename: attachment.filename.clone(),
                media_type: attachment.media_type.clone(),
                bytes: Vec::new(),
            })
            .collect();
        validate_send(&self.channel_id, &self.text, &attachments)?;
        validate_reply_to(self.reply_to.as_deref())?;
        // Check the whole input before reading any selected content.
        for source in &self.attachments {
            match (&source.file_id, &source.path) {
                (Some(id), None)
                    if id.len() == 32 && triblespace::prelude::Id::from_hex(id).is_some() => {}
                (Some(_), None) => {
                    bail!("file_id must be an exact 32-character Files entity or single-file import id")
                }
                (None, Some(path)) => {
                    crate::mcp::input::absolute(path)?;
                }
                _ => bail!("each attachment requires exactly one of file_id or path"),
            }
        }
        let mut remaining = MAX_SEND_ATTACHMENT_BYTES;
        for (source, attachment) in self.attachments.iter().zip(&mut attachments) {
            let bytes = if let Some(path) = &source.path {
                // A file past the remaining budget fails before it is read whole.
                crate::mcp::input::read_host_file(path, remaining as u64)
                    .context("attachments are limited to 20 MiB in total")?
            } else {
                // Exact entity ids take Files' scoped entity path. Hashes are
                // deliberately excluded: files_get may read unnamed resident
                // blobs, which need not belong to this workspace's Files view.
                crate::mcp::input::read_file_id(
                    files,
                    source.file_id.as_deref().expect("validated file source"),
                )?
            };
            if bytes.len() > remaining {
                bail!("attachments exceed the 20 MiB total upload limit");
            }
            attachment.bytes = bytes.as_ref().to_vec();
            remaining -= attachment.bytes.len();
        }
        Ok(attachments)
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Channels {
    guild: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthSet {
    secret_version: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}

#[derive(Clone, Debug)]
pub struct Discord {
    operations: super::operations::Discord,
    files: crate::files::operations::Files,
}
impl Discord {
    pub fn new(pile: PathBuf, key: Option<PathBuf>) -> Self {
        Self::with_storage(crate::storage::Storage::new(pile, key))
    }
    pub fn with_storage(storage: crate::storage::Storage) -> Self {
        Self {
            operations: super::operations::Discord::with_storage(storage.clone()),
            files: crate::files::operations::Files::with_storage(storage),
        }
    }
}
impl Faculty for Discord {
    fn tools(&self) -> &[Tool] {
        TOOLS
    }
    fn call(&self, name: &str, arguments: Bytes, out: &mut Out<'_>) -> Result<()> {
        match name {
            "discord_auth_status" => {
                let _: Empty = decode_arguments(arguments)?;
                render::auth(&self.operations.auth_status()?, out)
            }
            "discord_auth_set" => {
                let args: AuthSet = decode_arguments(arguments)?;
                let secret =
                    triblespace::prelude::Id::from_hex(&args.secret_version).ok_or_else(|| {
                        invalid_arguments("expected an exact 32-character Secrets version id")
                    })?;
                let revision = self.operations.auth_set(secret)?;
                out.line(format!(
                    "Discord auth revision {revision:X}; Secrets version {secret:X}"
                ))
            }
            "discord_read" => {
                let args: Read = decode_arguments(arguments)?;
                let options = ReadOptions {
                    channel_id: args.channel_id,
                    since: args.since,
                    limit: args.limit,
                    descending: args.descending,
                };
                options.validate().map_err(invalid_arguments)?;
                render::history(&self.operations.read(options)?, out)
            }
            "discord_pull" => {
                let args: Pull = decode_arguments(arguments)?;
                let options = PullOptions {
                    channel_id: args.channel_id,
                    fetch_limit: args.fetch_limit,
                    reconcile_limit: args.reconcile_limit,
                };
                options.validate().map_err(invalid_arguments)?;
                render::pull(&self.operations.pull(options)?, out)
            }
            "discord_send" => {
                let args: Send = decode_arguments(arguments)?;
                let attachments = args
                    .decode_attachments(&self.files)
                    .map_err(invalid_arguments)?;
                render::sent(
                    &self.operations.send_reply(
                        &args.channel_id,
                        &args.text,
                        &attachments,
                        args.reply_to.as_deref(),
                    )?,
                    out,
                )
            }
            "discord_channels_list" => {
                let args: Channels = decode_arguments(arguments)?;
                if let Some(guild) = &args.guild {
                    super::validate_snowflake(guild).map_err(invalid_arguments)?;
                }
                render::channels(&self.operations.channels_list(args.guild.as_deref())?, out)
            }
            _ => bail!("unknown Discord MCP tool {name:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use triblespace::core::metadata;
    use triblespace::prelude::*;

    #[test]
    fn send_reads_literal_text_and_host_path_attachments() {
        let files = crate::files::operations::Files::new(PathBuf::from("/not-opened"), None);
        let args: Send = serde_json::from_value(json!({"channel_id": "1", "text": "@-"})).unwrap();
        assert!(args.decode_attachments(&files).unwrap().is_empty());
        assert_eq!(args.text, "@-");
        let host = tempfile::tempdir().unwrap();
        let image = host.path().join("image.png");
        std::fs::write(&image, [0, 255, 10]).unwrap();
        let attachment = json!({"filename": "image.png", "media_type": "image/png", "path": image});
        let args: Send =
            serde_json::from_value(json!({"channel_id": "1", "attachments": [attachment.clone()]}))
                .unwrap();
        assert!(args.text.is_empty());
        assert_eq!(
            args.decode_attachments(&files).unwrap()[0].bytes,
            [0, 255, 10]
        );
        for invalid in ["relative.png", "/no/such/image.png"] {
            let mut attachment = attachment.clone();
            attachment["path"] = json!(invalid);
            let args: Send =
                serde_json::from_value(json!({"channel_id": "1", "attachments": [attachment]}))
                    .unwrap();
            assert!(args.decode_attachments(&files).is_err());
        }
        let mut retired = attachment.clone();
        retired["data_base64"] = json!("AP8K");
        assert!(serde_json::from_value::<Send>(
            json!({"channel_id": "1", "attachments": [retired]})
        )
        .is_err());
        let args: Send = serde_json::from_value(json!({"channel_id": "1"})).unwrap();
        assert!(args.decode_attachments(&files).is_err());
    }
    #[test]
    fn send_reads_a_large_retained_file_without_base64_transport() {
        crate::test_support::clear_ambient_environment();
        let directory = tempfile::tempdir().unwrap();
        let pile = directory.path().join("send.pile");
        let key = directory.path().join("send.key");
        std::fs::File::create(&pile).unwrap();
        crate::storage::initialize_signer(&pile, Some(&key)).unwrap();
        let files = crate::files::operations::Files::new(pile.clone(), Some(key.clone()));
        let bytes = vec![0x42; 2_600_000];
        let id = files
            .add_bytes(
                Bytes::from_source(bytes.clone()),
                "scan.pdf",
                "application/pdf",
                &[],
            )
            .unwrap();
        let raw = json!({"channel_id": "1", "attachments": [{
            "filename": "scan.pdf", "media_type": "application/pdf", "file_id": format!("{id:x}")
        }]});
        assert!(raw.to_string().len() < 1024);
        let args: Send = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(args.decode_attachments(&files).unwrap()[0].bytes, bytes);
        // Files.get follows an import with one file root. The MCP selector
        // keeps that scoped resolution behavior rather than rejecting imports.
        let import = entity! { _ @
            metadata::tag: &crate::schemas::files::KIND_IMPORT,
            crate::schemas::files::file::root: id,
        };
        let import_id = import.root().unwrap();
        crate::storage::publish_fragment(
            &pile,
            Some(&key),
            crate::schemas::files::DEFAULT_SCOPE_ID,
            import,
        )
        .unwrap();
        let mut via_import = raw.clone();
        via_import["attachments"][0]["file_id"] = json!(format!("{import_id:x}"));
        let args: Send = serde_json::from_value(via_import).unwrap();
        assert_eq!(args.decode_attachments(&files).unwrap()[0].bytes, bytes);
        let mut invalid = raw.clone();
        invalid["attachments"][0]["path"] = json!("/no/such/scan.pdf");
        let args: Send = serde_json::from_value(invalid).unwrap();
        assert!(args
            .decode_attachments(&files)
            .unwrap_err()
            .to_string()
            .contains("exactly one"));
        // Content hashes and host paths cannot select the broader unnamed-blob fallback.
        for selector in ["f".repeat(64), "/etc/passwd".to_owned()] {
            let mut invalid = raw.clone();
            invalid["attachments"][0]["file_id"] = json!(selector);
            let args: Send = serde_json::from_value(invalid).unwrap();
            assert!(args.decode_attachments(&files).is_err());
        }
        // An entity absent from the selected Files view cannot be fetched.
        let mut invalid = raw;
        invalid["attachments"][0]["file_id"] =
            json!(format!("{:x}", triblespace::prelude::ufoid().id));
        let args: Send = serde_json::from_value(invalid).unwrap();
        assert!(args.decode_attachments(&files).is_err());
    }
}
