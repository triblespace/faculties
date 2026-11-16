//! Tailored Mail CLI: host files/password inputs and ambient persona are decoded here.
use super::{operations::*, render};
use crate::out::Out;
use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use std::{
    fs,
    path::{Path, PathBuf},
};
use triblespace::prelude::Id;

#[derive(Parser)]
#[command(version = crate::GIT_VERSION, name = "mail", about = "Immutable email evidence, drafts, and delivery receipts")]
pub struct Cli {
    #[command(flatten)]
    workspace: crate::cli_workspace::WorkspaceArgs,
    #[arg(long, env = "PILE")]
    pile: PathBuf,
    /// Existing durable collection signer. Ordinary commands never create it.
    #[arg(long, env = "TRIBLESPACE_KEY")]
    key: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Manage immutable full-state mail-account configurations.
    Account {
        #[command(subcommand)]
        command: AccountCommand,
    },
    /// Observe enabled accounts read-only over IMAP (EXAMINE + BODY.PEEK[]).
    /// Never flags, moves or deletes anything on the server.
    Fetch(FetchArgs),
    /// Create one immutable draft and its deterministic Decide proposal.
    Draft(DraftArgs),
    /// Create a reply draft from an inbound wire message.
    Reply(ReplyArgs),
    /// Submit one authorized draft under externally serialized execution.
    Send { draft: String },
    /// List immutable drafts and their delivery state.
    Outbox,
    /// List inbound projections for the configured persona.
    List {
        #[arg(long)]
        unread: bool,
        #[arg(long)]
        spam: bool,
    },
    /// Record intrinsic read evidence for one inbound wire message.
    Read { message: String },
    /// Show one inbound or outgoing wire message.
    Show { message: String },
    /// Case-insensitive substring search over projected subject and body.
    Search { query: String },
    /// List one wire message's attachments; with --save, write their exact
    /// bytes into a directory. Never overwrites a file and records nothing.
    Attachments {
        message: String,
        /// Directory to write each attachment into, by its original name.
        #[arg(long)]
        save: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum AccountCommand {
    /// Add an account or append a full-state successor to an existing account.
    Set {
        /// Existing account id/address. Omit only when creating a new anchor.
        #[arg(long)]
        account: Option<String>,
        #[arg(long)]
        address: String,
        #[arg(long)]
        display_name: String,
        /// Implicit-TLS IMAP endpoint as `host:port` (normally port 993).
        #[arg(long)]
        imap_endpoint: String,
        /// Implicit-TLS endpoint as `host:port`.
        #[arg(long)]
        smtp_endpoint: String,
        #[arg(long)]
        username: Option<String>,
        /// Mailbox secret. Prefer MAIL_PASS rather than a visible argv value.
        #[arg(long, env = "MAIL_PASS", hide_env_values = true)]
        password: Option<String>,
        /// Exact existing Secrets version. This is the repair path when
        /// a Secrets-first account update was interrupted before Mail commit.
        #[arg(long, value_parser = parse_id, conflicts_with = "password")]
        credential_version: Option<Id>,
        #[arg(long)]
        disabled: bool,
        /// Store this mailbox's inbound mail without waking anyone: an
        /// archive fetched for bookkeeping, not an inbox to answer.
        #[arg(long)]
        quiet: bool,
    },
    List,
}

#[derive(Args)]
struct FetchArgs {
    /// Server mailbox name to examine; repeatable. Defaults to INBOX.
    #[arg(long = "mailbox", conflicts_with = "all_mailboxes")]
    mailboxes: Vec<String>,
    /// Examine every selectable mailbox the server lists.
    #[arg(long)]
    all_mailboxes: bool,
    /// IMAP SEARCH FROM: substring of the From header.
    #[arg(long)]
    from: Option<String>,
    /// IMAP SEARCH SUBJECT: substring of the Subject header.
    #[arg(long)]
    subject: Option<String>,
    /// IMAP SEARCH SINCE: received on or after this day (YYYY-MM-DD or 1-Jan-2024).
    #[arg(long, value_parser = parse_date)]
    since: Option<super::SearchDate>,
    /// IMAP SEARCH BEFORE: received before this day (YYYY-MM-DD or 1-Jan-2024).
    #[arg(long, value_parser = parse_date)]
    before: Option<super::SearchDate>,
}

impl FetchArgs {
    fn request(self) -> FetchRequest {
        FetchRequest {
            mailboxes: if self.all_mailboxes {
                super::MailboxSelection::All
            } else if self.mailboxes.is_empty() {
                super::MailboxSelection::Inbox
            } else {
                super::MailboxSelection::Named(self.mailboxes)
            },
            criteria: super::SearchCriteria {
                from: self.from,
                subject: self.subject,
                since: self.since,
                before: self.before,
            },
        }
    }
}

#[derive(Args)]
struct DraftArgs {
    /// Account id/address; required even when only one account exists.
    #[arg(long)]
    account: String,
    #[arg(long, required = true)]
    to: Vec<String>,
    #[arg(long)]
    cc: Vec<String>,
    #[arg(long)]
    bcc: Vec<String>,
    #[arg(long)]
    subject: String,
    /// Literal text, `@path`, or `@-`.
    body: String,
    #[arg(long)]
    attach: Vec<PathBuf>,
}

#[derive(Args)]
struct ReplyArgs {
    message: String,
    #[arg(long)]
    account: String,
    /// Literal text, `@path`, or `@-`.
    body: String,
}

fn attachment(path: &Path) -> Result<DraftAttachment> {
    let bytes = fs::read(path).with_context(|| format!("read attachment {}", path.display()))?;
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("attachment.bin")
        .to_owned();
    Ok(DraftAttachment::Resident(super::AttachmentData {
        filename,
        media_type: crate::files::infer_media_type(path).to_owned(),
        bytes,
    }))
}
fn persona() -> Result<String> {
    std::env::var("PERSONA").context("PERSONA must select an active Relations person")
}
/// One-shot storage: every operation opens and closes its own store.
pub fn execute(cli: Cli, out: &mut Out<'_>) -> Result<()> {
    let storage = cli.workspace.storage(cli.pile.clone(), cli.key.clone())?;
    execute_in(&storage, cli, out)
}

/// The process's whole body: one store, closed for process exit.
fn execute_to_exit(cli: Cli, out: &mut Out<'_>) -> Result<()> {
    cli.workspace
        .storage(cli.pile.clone(), cli.key.clone())?
        .scope_to_exit(|storage| execute_in(storage, cli, out))
}

fn execute_in(storage: &crate::storage::Storage, cli: Cli, out: &mut Out<'_>) -> Result<()> {
    let operations = Mail::with_storage(storage.clone());
    match cli.command {
        Command::Account { command } => match command {
            AccountCommand::Set {
                account,
                address,
                display_name,
                imap_endpoint,
                smtp_endpoint,
                username,
                password,
                credential_version,
                disabled,
                quiet,
            } => {
                let input = AccountOptions {
                    account,
                    address,
                    display_name,
                    imap_endpoint,
                    smtp_endpoint,
                    username,
                    credential_version,
                    disabled,
                    quiet,
                };
                let value = match password {
                    Some(password) => operations.account_set_with_password(input, password)?,
                    None => operations.account_set(input)?,
                };
                for notice in &value.notices {
                    eprintln!("{notice}");
                }
                render::account_set(&value, out)
            }
            AccountCommand::List => render::accounts(&operations.account_list()?, out),
        },
        Command::Fetch(args) => render::fetched(&operations.fetch(args.request())?, out),
        Command::Draft(args) => {
            let body = crate::text_arg(&args.body, "draft body")?;
            let attachments = args
                .attach
                .iter()
                .map(|path| attachment(path))
                .collect::<Result<Vec<_>>>()?;
            render::draft(
                &operations.draft(DraftRequest {
                    account: args.account,
                    to: args.to,
                    cc: args.cc,
                    bcc: args.bcc,
                    subject: args.subject,
                    body,
                    attachments,
                })?,
                out,
            )
        }
        Command::Reply(args) => render::draft(
            &operations.reply(ReplyRequest {
                message: args.message,
                account: args.account,
                body: crate::text_arg(&args.body, "reply body")?,
            })?,
            out,
        ),
        Command::Send { draft } => render::sent(&operations.send(&draft)?, out),
        Command::Outbox => render::outbox(&operations.outbox()?, out),
        Command::List { unread, spam } => {
            render::inbox(&operations.list(&persona()?, unread, spam)?, out)
        }
        Command::Read { message } => render::read(&operations.read(&persona()?, &message)?, out),
        Command::Show { message } => render::show(&operations.show(&message)?, out),
        Command::Search { query } => render::search(&operations.search(&query)?, out),
        Command::Attachments { message, save } => {
            let listings = operations.attachments(&message)?;
            let Some(directory) = save else {
                return render::attachments(&listings, None, out);
            };
            let saved = save_attachments(&directory, &listings)?;
            render::attachments(&listings, Some(&saved), out)?;
            let failed = saved
                .iter()
                .filter(|value| matches!(value, Saved::Refused(_)))
                .count();
            if failed > 0 {
                anyhow::bail!("{failed} attachment(s) were not saved; no file was overwritten");
            }
            Ok(())
        }
    }
}

/// Where one listed attachment's bytes went under `mail attachments --save`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Saved {
    /// Written now, to a file that did not exist.
    Written(PathBuf),
    /// A file with exactly these bytes was already there.
    Present(PathBuf),
    /// Not resident, so nothing to write.
    Skipped,
    /// Every candidate name held different bytes; nothing was written.
    Refused(String),
}

/// A filesystem-safe leaf name: path separators and control characters
/// become `_`, and an empty (or dot-only) name becomes `attachment-<ordinal>.bin`.
pub(super) fn sanitize_filename(name: &str, ordinal: u64) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c == '/' || c == '\\' || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        format!("attachment-{ordinal}.bin")
    } else {
        cleaned
    }
}

/// `name` with `-<ordinal>` before its extension (`a.pdf` becomes `a-1.pdf`).
fn suffixed(name: &str, ordinal: u64) -> String {
    match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => format!("{stem}-{ordinal}.{extension}"),
        _ => format!("{name}-{ordinal}"),
    }
}

/// Write each resident attachment to `directory/<sanitized name>`, or to
/// `<stem>-<ordinal>.<ext>` when that name holds other bytes. An existing file
/// is never overwritten; one holding exactly these bytes counts as saved.
pub(super) fn save_attachments(
    directory: &Path,
    listings: &[AttachmentListing],
) -> Result<Vec<Saved>> {
    fs::create_dir_all(directory)
        .with_context(|| format!("create attachment directory {}", directory.display()))?;
    listings
        .iter()
        .map(|listing| {
            let AttachmentState::Resident {
                filename, bytes, ..
            } = &listing.state
            else {
                return Ok(Saved::Skipped);
            };
            let ordinal = listing.ordinal.unwrap_or_default();
            let name = sanitize_filename(filename, ordinal);
            for candidate in [name.clone(), suffixed(&name, ordinal)] {
                let path = directory.join(&candidate);
                match fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)
                {
                    Ok(mut file) => {
                        use std::io::Write;
                        if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
                            drop(file);
                            let _ = fs::remove_file(&path);
                            return Err(error)
                                .with_context(|| format!("write attachment {}", path.display()));
                        }
                        return Ok(Saved::Written(path));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        let same = fs::metadata(&path)
                            .is_ok_and(|meta| meta.is_file() && meta.len() == bytes.len() as u64)
                            && fs::read(&path).is_ok_and(|held| held == bytes.as_ref());
                        if same {
                            return Ok(Saved::Present(path));
                        }
                    }
                    Err(error) => {
                        return Err(error)
                            .with_context(|| format!("create attachment {}", path.display()))
                    }
                }
            }
            Ok(Saved::Refused(format!(
                "{name} and {} already hold other bytes",
                suffixed(&name, ordinal)
            )))
        })
        .collect()
}
pub fn run() -> Result<()> {
    let cli = Cli::parse();
    crate::cli::with_output("mail", |out| execute_to_exit(cli, out))
}
fn parse_date(raw: &str) -> std::result::Result<super::SearchDate, String> {
    super::SearchDate::parse(raw)
        .ok_or_else(|| format!("'{raw}' is not a date (YYYY-MM-DD or D-Mon-YYYY)"))
}
fn parse_id(raw: &str) -> std::result::Result<Id, String> {
    Id::from_hex(raw.trim())
        .ok_or_else(|| format!("'{raw}' is not one exact nonzero 32-digit hexadecimal id"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cli_rejects_legacy_secret_identity_and_scope_surface() {
        assert!(Cli::try_parse_from([
            "mail",
            "--pile",
            "/tmp/not-opened-mail-test.pile",
            "--secrets-identity",
            "operator",
            "account",
            "list",
        ])
        .is_err());
        assert!(Cli::try_parse_from([
            "mail",
            "--pile",
            "/tmp/not-opened-mail-test.pile",
            "account",
            "set",
            "--address",
            "me@example.test",
            "--display-name",
            "Me",
            "--imap-endpoint",
            "imap.example.test:993",
            "--smtp-endpoint",
            "smtp.example.test:465",
            "--password",
            "secret",
            "--secret-scope",
            "mail-test",
        ])
        .is_err());
    }

    #[test]
    fn saved_attachment_names_stay_inside_the_directory() {
        assert_eq!(sanitize_filename("invoice.pdf", 0), "invoice.pdf");
        assert_eq!(sanitize_filename("../a/b\\c\u{7}.pdf", 2), ".._a_b_c_.pdf");
        assert_eq!(sanitize_filename("", 3), "attachment-3.bin");
        assert_eq!(sanitize_filename("..", 4), "attachment-4.bin");
        assert_eq!(suffixed("invoice.pdf", 1), "invoice-1.pdf");
        assert_eq!(suffixed("archive.tar.gz", 1), "archive.tar-1.gz");
        assert_eq!(suffixed(".hidden", 2), ".hidden-2");
        assert_eq!(suffixed("README", 5), "README-5");
    }

    #[test]
    fn fetch_flags_become_one_read_only_request() {
        let parse = |extra: &[&str]| {
            let mut argv = vec!["mail", "--pile", "/tmp/not-opened-mail-test.pile", "fetch"];
            argv.extend_from_slice(extra);
            match Cli::try_parse_from(argv).map(|cli| cli.command) {
                Ok(Command::Fetch(args)) => Ok(args.request()),
                Ok(_) => panic!("not a fetch"),
                Err(error) => Err(error),
            }
        };
        let request = parse(&[]).unwrap();
        assert_eq!(request.mailboxes, super::super::MailboxSelection::Inbox);
        assert!(request.criteria.is_empty());

        let request = parse(&[
            "--all-mailboxes",
            "--from",
            "anthropic",
            "--since",
            "2024-01-01",
            "--before",
            "1-Feb-2024",
        ])
        .unwrap();
        assert_eq!(request.mailboxes, super::super::MailboxSelection::All);
        assert_eq!(request.criteria.from.as_deref(), Some("anthropic"));
        assert_eq!(request.criteria.since.unwrap().imap_text(), "1-Jan-2024");
        assert_eq!(request.criteria.before.unwrap().imap_text(), "1-Feb-2024");

        let request = parse(&["--mailbox", "INBOX", "--mailbox", "Archive"]).unwrap();
        assert_eq!(
            request.mailboxes,
            super::super::MailboxSelection::Named(vec!["INBOX".into(), "Archive".into()])
        );
        assert!(parse(&["--mailbox", "INBOX", "--all-mailboxes"]).is_err());
        assert!(parse(&["--since", "last tuesday"]).is_err());
    }

    #[test]
    fn cli_password_uses_configured_secrets_collection() {
        let command = [
            "mail",
            "--pile",
            "/tmp/not-opened-mail-test.pile",
            "account",
            "set",
            "--address",
            "me@example.test",
            "--display-name",
            "Me",
            "--imap-endpoint",
            "imap.example.test:993",
            "--smtp-endpoint",
            "smtp.example.test:465",
            "--password",
            "secret",
        ];
        assert!(Cli::try_parse_from(command).is_ok());

        let mut with_pop = command.to_vec();
        with_pop[9] = "--pop-endpoint";
        assert!(Cli::try_parse_from(with_pop).is_err());

        let mut with_vault = command.to_vec();
        with_vault.extend(["--vault", "78787878787878787878787878787878"]);
        assert!(Cli::try_parse_from(with_vault).is_err());
    }
}
