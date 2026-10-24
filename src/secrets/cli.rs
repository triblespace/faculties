//! Shell-specific secret input and exact stdout export.
use super::Secrets;
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use ed25519_dalek::VerifyingKey;
use faculties_secrets::encrypted::Encrypted;
use faculties_secrets::item::{AttachmentInput, FieldInput, FieldValue, ItemInput};
use faculties_secrets::resource::{DeliveryLimits, SecretTarget};
use faculties_secrets::schema::{ROLE_NOTES, ROLE_PASSWORD, ROLE_URL, ROLE_USERNAME};
use hifitime::Epoch;
use std::io::{Read, Write};
use std::path::PathBuf;
use triblespace::core::collection::CollectionHandle;
use triblespace::prelude::Id;
use triblespace::prelude::{blobencodings, inlineencodings, Blob, Inline, IntoBlob};
use zeroize::Zeroizing;
#[derive(Parser)]
#[command(
    version = crate::GIT_VERSION,
    name = "secrets",
    about = "Immutable encrypted versions in one capability-governed collection"
)]
struct Cli {
    /// Path to the pile file.
    #[arg(long, env = "PILE")]
    pile: PathBuf,
    /// Existing durable node signing-key file. Commands never create it.
    #[arg(long, env = "TRIBLESPACE_KEY")]
    key: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Ordinary node/agent-readable 1PUX import; retains exact source and public item titles.
    /// For user-held protection, use the separate local app import instead.
    #[command(name = "import-1password")]
    Import1password {
        /// Local unencrypted export. Read in memory, never extracted or removed.
        path: PathBuf,
        /// Validate and report counts only; never open the destination or keys.
        #[arg(long, conflicts_with = "recipient")]
        check: bool,
        /// Explicit recipients for exact archive/item resources; no delegation
        /// or collection access. The internal retry receipt remains owner-only.
        #[arg(long, value_parser = parse_recipient)]
        recipient: Vec<VerifyingKey>,
    },
    /// Add a login. Only the explicit label is public; values come from files
    /// (use '-' for at most one stdin input), never literal secret argv values.
    AddLogin {
        #[arg(long)]
        label: String,
        #[arg(long)]
        username_file: Option<PathBuf>,
        #[arg(long)]
        password_file: PathBuf,
        #[arg(long)]
        url_file: Option<PathBuf>,
        #[arg(long)]
        notes_file: Option<PathBuf>,
        /// File contents and basename are both encrypted.
        #[arg(long)]
        attachment: Vec<PathBuf>,
    },
    /// List structured versions using public labels only.
    Items,
    /// List decryptable body alternatives; --body explicitly exports one native
    /// SimpleArchive. Child handles remain ciphertext, selectable with item-export.
    ItemOpen {
        #[arg(long, value_parser = parse_id)]
        secret: Id,
        #[arg(long, value_parser = parse_child)]
        body: Option<[u8; 32]>,
    },
    /// Inspect native roles and typed ciphertext child/name handles, not values.
    ItemInspect {
        #[arg(long, value_parser = parse_id)]
        secret: Id,
        #[arg(long, value_parser = parse_child)]
        body: [u8; 32],
    },
    /// Explicitly export one selected ciphertext child as exact plaintext bytes.
    ItemExport {
        #[arg(long, value_parser = parse_id)]
        secret: Id,
        #[arg(long, value_parser = parse_child)]
        body: [u8; 32],
        #[arg(long, value_parser = parse_child)]
        child: [u8; 32],
        /// The selected child has UTF8String rather than RawBytes encoding.
        #[arg(long)]
        text: bool,
    },
    /// Generate a current TOTP from one exact encrypted text field. JSON output
    /// includes Unix-second validity bounds and identity, never seed/full URI.
    ItemTotp {
        #[arg(long, value_parser = parse_id)]
        secret: Id,
        #[arg(long, value_parser = parse_child)]
        body: [u8; 32],
        #[arg(long, value_parser = parse_child)]
        child: [u8; 32],
    },
    /// Add one immutable encrypted version.
    Add {
        #[arg(long)]
        name: String,
        /// Literal value, `@file`, or `@-` for stdin.
        #[arg(long)]
        value: String,
    },
    /// Open one exact immutable version id.
    Get {
        #[arg(long, value_parser = parse_id)]
        secret: Id,
    },
    /// List complete immutable versions in the configured collection.
    List,
    /// Grant future DEK delivery for one immutable resource (not collection READ/WRITE).
    Grant {
        #[arg(long, value_parser = parse_id, required_unless_present = "resource", conflicts_with = "resource")]
        secret: Option<Id>,
        #[arg(long, value_parser = parse_resource, required_unless_present = "secret")]
        resource: Option<CollectionHandle>,
        #[arg(long, value_parser = parse_recipient)]
        recipient: VerifyingKey,
        /// Earliest future delivery instant, inclusive (RFC3339).
        #[arg(long)]
        not_before: Option<Epoch>,
        /// End of future delivery, exclusive (RFC3339); existing envelopes still open.
        #[arg(long)]
        expires_at: Option<Epoch>,
        /// Also allow the recipient to delegate this delivery action.
        #[arg(long)]
        delegate: bool,
    },
    /// Deliver DEKs for selected resources; without a selection, visit held bound resources.
    Maintain {
        #[arg(long, value_parser = parse_id)]
        secret: Vec<Id>,
        #[arg(long, value_parser = parse_resource)]
        resource: Vec<CollectionHandle>,
    },
}

fn parse_id(raw: &str) -> std::result::Result<Id, String> {
    Id::from_hex(raw.trim())
        .ok_or_else(|| format!("'{raw}' is not one exact nonzero 32-digit hexadecimal id"))
}

fn parse_child(raw: &str) -> std::result::Result<[u8; 32], String> {
    hex::decode(raw.strip_prefix("blake3:").unwrap_or(raw))
        .map_err(|_| "child must be a 64-digit ciphertext handle".to_owned())?
        .try_into()
        .map_err(|_| "child must be a 64-digit ciphertext handle".to_owned())
}

fn read_private_input(path: &std::path::Path) -> Result<Zeroizing<Vec<u8>>> {
    if path == std::path::Path::new("-") {
        let mut bytes = Zeroizing::new(Vec::new());
        std::io::stdin()
            .read_to_end(&mut bytes)
            .context("read private input from stdin")?;
        Ok(bytes)
    } else {
        std::fs::read(path)
            .map(Zeroizing::new)
            .context("read private input file")
    }
}

fn write_plaintext(bytes: &[u8]) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(bytes)
        .context("write explicit plaintext export")?;
    stdout.flush().context("flush explicit plaintext export")
}

fn load_export(path: &std::path::Path, limit: usize) -> Result<Zeroizing<Vec<u8>>> {
    let file = std::fs::File::open(path)
        .map_err(|_| anyhow::anyhow!("1Password import: cannot open export"))?;
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| anyhow::anyhow!("1Password import: cannot read export"))?;
    anyhow::ensure!(
        bytes.len() <= limit,
        "1Password import: archive byte limit exceeded"
    );
    Ok(bytes)
}

fn write_import_counts(out: &mut impl Write, c: super::onepassword::ImportCounts) -> Result<()> {
    writeln!(out, "accounts {}; vaults {}; items {}; fields {}; attachments {}; files {}; archive-only files {}; archive-only scalar values {}",
        c.accounts, c.vaults, c.items, c.fields, c.attachments, c.files,
        c.archive_only_files, c.archive_only_values).context("write private import counts")
}

pub(crate) fn parse_resource(raw: &str) -> std::result::Result<CollectionHandle, String> {
    let raw = raw.trim().strip_prefix("blake3:").unwrap_or(raw.trim());
    let bytes: [u8; 32] = hex::decode(raw)
        .map_err(|_| "resource must be a 64-digit blob handle")?
        .try_into()
        .map_err(|_| "resource must be a 64-digit blob handle")?;
    Ok(CollectionHandle::new(bytes))
}

pub(crate) fn parse_recipient(raw: &str) -> std::result::Result<VerifyingKey, String> {
    let bytes: [u8; 32] = hex::decode(raw.trim())
        .map_err(|_| "recipient must be a 64-digit public key")?
        .try_into()
        .map_err(|_| "recipient must be a 64-digit public key")?;
    let key = VerifyingKey::from_bytes(&bytes).map_err(|_| "invalid Ed25519 recipient")?;
    if key.is_weak() || key.to_edwards().compress().to_bytes() != bytes {
        return Err("recipient must be a canonical, non-weak Ed25519 key".into());
    }
    Ok(key)
}

fn load_value(raw: String) -> Result<Zeroizing<Vec<u8>>> {
    if let Some(path) = raw.strip_prefix('@') {
        if path == "-" {
            let mut value = Vec::new();
            std::io::stdin()
                .read_to_end(&mut value)
                .context("read secret value from stdin")?;
            Ok(Zeroizing::new(value))
        } else {
            std::fs::read(path)
                .map(Zeroizing::new)
                .with_context(|| format!("read {path}"))
        }
    } else {
        Ok(Zeroizing::new(raw.into_bytes()))
    }
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let operations = Secrets::new(cli.pile, cli.key);
    match cli.command {
        Command::Import1password {
            path,
            recipient,
            check,
        } => {
            let limits = super::onepassword::ImportLimits::default();
            let bytes = load_export(&path, limits.archive_bytes)?;
            // Check is deliberately before any destination store/key access.
            let counts = if check {
                anyhow::ensure!(recipient.is_empty(), "--check does not deliver recipients");
                super::onepassword::OnePasswordArchive::parse(&bytes, limits)?.counts()
            } else {
                let report = operations.import_1password(&bytes, limits)?;
                let mut stdout = std::io::stdout().lock();
                writeln!(
                    stdout,
                    "{} archive {:x}; receipt {:x}; views {}",
                    if report.reused {
                        "existing"
                    } else {
                        "imported"
                    },
                    report.archive,
                    report.receipt,
                    if report.view_maintenance_pending {
                        "pending"
                    } else {
                        "ready"
                    }
                )?;
                write_import_counts(&mut stdout, report.counts)?;
                if !recipient.is_empty() {
                    let targets = report
                        .delivery_resources
                        .iter()
                        .copied()
                        .map(SecretTarget::Resource)
                        .collect::<Vec<_>>();
                    let delivery = operations.grant_selected(&targets, &recipient, DeliveryLimits::default())
                        .map_err(|_| anyhow::anyhow!(
                            "1Password import committed; recipient delivery incomplete; retry the same export and recipients"
                        ))?;
                    writeln!(
                        stdout,
                        "delivery complete; added {} envelope(s); views {}",
                        delivery.added_envelopes,
                        if delivery.view_maintenance_pending {
                            "pending"
                        } else {
                            "ready"
                        }
                    )?;
                }
                return stdout.flush().context("flush private import report");
            };
            let mut stdout = std::io::stdout().lock();
            write_import_counts(&mut stdout, counts)?;
            stdout.flush().context("flush private import check")
        }
        Command::ItemOpen { secret, body } => {
            if let Some(body) = body {
                let facts = operations.open_item_body(secret, Inline::new(body))?;
                let blob: Blob<blobencodings::SimpleArchive> = facts.to_blob();
                write_plaintext(blob.bytes.as_ref())
            } else {
                for body in operations.open_item(secret)? {
                    writeln!(std::io::stdout().lock(), "blake3:{}", hex::encode(body.raw))?;
                }
                Ok(())
            }
        }
        Command::ItemInspect { secret, body } => {
            let mut stdout = std::io::stdout().lock();
            operations.inspect_item(
                secret,
                Inline::new(body),
                &mut crate::out::Out::new(&mut |part| match part {
                    crate::out::Part::Text { text } => stdout
                        .write_all(text.as_bytes())
                        .context("write explicit item inspection"),
                    _ => unreachable!("inspection emits native text only"),
                }),
            )?;
            stdout.flush().context("flush explicit item inspection")
        }
        Command::ItemTotp {
            secret,
            body,
            child,
        } => {
            let code = operations.item_totp(secret, Inline::new(body), Inline::new(child))?;
            let mut stdout = std::io::stdout().lock();
            writeln!(stdout, "{}", super::operations::totp_json(&code)?)?;
            stdout.flush().context("flush explicit TOTP output")
        }
        Command::ItemExport {
            secret,
            body,
            child,
            text,
        } => {
            if text {
                let handle =
                    Inline::<inlineencodings::Handle<Encrypted<blobencodings::UTF8String>>>::new(
                        child,
                    );
                write_plaintext(
                    operations
                        .open_item_child(secret, Inline::new(body), handle)?
                        .bytes
                        .as_ref(),
                )
            } else {
                let handle =
                    Inline::<inlineencodings::Handle<Encrypted<blobencodings::RawBytes>>>::new(
                        child,
                    );
                write_plaintext(
                    operations
                        .open_item_child(secret, Inline::new(body), handle)?
                        .bytes
                        .as_ref(),
                )
            }
        }
        // An explicit secret export is never sensory output, even with Drive.
        Command::Get { secret } => {
            let plaintext = operations.get(secret)?;
            let mut stdout = std::io::stdout().lock();
            stdout
                .write_all(&plaintext)
                .context("write secret to stdout")?;
            stdout.flush().context("flush secret stdout")
        }
        command => crate::cli::with_output("secrets", |out| match command {
            Command::AddLogin {
                label,
                username_file,
                password_file,
                url_file,
                notes_file,
                attachment,
            } => {
                let paths = [
                    (ROLE_USERNAME, username_file.as_deref()),
                    (ROLE_PASSWORD, Some(password_file.as_path())),
                    (ROLE_URL, url_file.as_deref()),
                    (ROLE_NOTES, notes_file.as_deref()),
                ];
                anyhow::ensure!(
                    paths
                        .iter()
                        .filter(|(_, path)| *path == Some(std::path::Path::new("-")))
                        .count()
                        <= 1,
                    "only one private input may use stdin"
                );
                let mut values = Vec::new();
                for (role, path) in paths {
                    if let Some(path) = path {
                        let bytes = read_private_input(path)?;
                        std::str::from_utf8(&bytes).context("private login field is not UTF-8")?;
                        values.push((role, bytes));
                    }
                }
                let fields = values
                    .iter()
                    .map(|(role, bytes)| FieldInput {
                        role: *role,
                        label: None,
                        value: FieldValue::Text(
                            std::str::from_utf8(bytes).expect("validated UTF-8"),
                        ),
                    })
                    .collect::<Vec<_>>();
                let mut files = Vec::new();
                for path in attachment {
                    let filename = path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .context("private attachment needs a UTF-8 filename")?
                        .to_owned();
                    let bytes = std::fs::read(path)
                        .map(Zeroizing::new)
                        .context("read private attachment")?;
                    files.push((filename, bytes));
                }
                let attachments = files
                    .iter()
                    .map(|(filename, bytes)| AttachmentInput { filename, bytes })
                    .collect::<Vec<_>>();
                let secret = operations.add_item(
                    &label,
                    ItemInput {
                        fields: &fields,
                        attachments: &attachments,
                    },
                )?;
                out.line(format!("item {secret:x}  {label}"))
            }
            Command::Items => {
                let rows = operations.list_items()?;
                if rows.is_empty() {
                    out.line("(no structured items)")?;
                }
                for row in rows {
                    match row.name {
                        Some(name) => out.line(format!(
                            "{:x} body blake3:{} public-name {name}",
                            row.id,
                            hex::encode(row.body.raw)
                        ))?,
                        None => out.line(format!(
                            "{:x} body blake3:{} public-name-unavailable",
                            row.id,
                            hex::encode(row.body.raw)
                        ))?,
                    }
                }
                Ok(())
            }
            Command::Add { name, value } => {
                let plaintext = load_value(value)?;
                let secret = operations.add(&name, &plaintext)?;
                out.line(format!("secret {secret:x}  {name}"))
            }
            Command::List => {
                let rows = operations.list()?;
                if rows.is_empty() {
                    out.line("(no secrets)")?;
                }
                for row in rows {
                    out.line(format!("{:x}  {}", row.id, row.name))?;
                }
                Ok(())
            }
            Command::Grant {
                secret,
                resource,
                recipient,
                not_before,
                expires_at,
                delegate,
            } => {
                let target = match (secret, resource) {
                    (Some(secret), None) => SecretTarget::Secret(secret),
                    (None, Some(resource)) => SecretTarget::Resource(resource),
                    _ => unreachable!("clap enforces exactly one target"),
                };
                let ids = operations.grant(
                    target,
                    recipient,
                    DeliveryLimits {
                        not_before,
                        expires_at,
                    },
                    delegate,
                )?;
                for id in ids {
                    out.line(format!("AUTH blake3:{}", hex::encode(id.raw)))?;
                }
                Ok(())
            }
            Command::Maintain { secret, resource } => out.line(format!(
                "added {} recipient envelope(s)",
                operations.maintain_selected(
                    &secret
                        .into_iter()
                        .map(SecretTarget::Secret)
                        .chain(resource.into_iter().map(SecretTarget::Resource))
                        .collect::<Vec<_>>()
                )?
            )),
            Command::Get { .. }
            | Command::Import1password { .. }
            | Command::ItemOpen { .. }
            | Command::ItemExport { .. }
            | Command::ItemTotp { .. }
            | Command::ItemInspect { .. } => {
                unreachable!()
            }
        }),
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn totp_cli_requires_exact_selectors_and_has_no_clock_or_seed_input() {
        let secret = "01".repeat(16);
        let handle = "01".repeat(32);
        let args = [
            "secrets",
            "--pile",
            "unused",
            "item-totp",
            "--secret",
            &secret,
            "--body",
            &handle,
            "--child",
            &handle,
        ];
        assert!(Cli::try_parse_from(args).is_ok());
        assert!(Cli::try_parse_from(&args[..8]).is_err());
        for extra in ["--unix-seconds", "--seed", "--value", "--expected-issuer"] {
            assert!(Cli::try_parse_from(args.into_iter().chain([extra, "unsupported"])).is_err());
        }
    }

    #[test]
    fn import_check_is_explicit_and_cannot_grant_recipients() {
        assert!(Cli::try_parse_from([
            "secrets",
            "--pile",
            "unused",
            "import-1password",
            "synthetic.1pux",
            "--check"
        ])
        .is_ok());
        assert!(Cli::try_parse_from([
            "secrets",
            "--pile",
            "unused",
            "import-1password",
            "synthetic.1pux",
            "--password",
            "unsupported"
        ])
        .is_err());
        let public = hex::encode(
            ed25519_dalek::SigningKey::from_bytes(&[78; 32])
                .verifying_key()
                .to_bytes(),
        );
        assert!(Cli::try_parse_from([
            "secrets",
            "--pile",
            "unused",
            "import-1password",
            "synthetic.1pux",
            "--check",
            "--recipient",
            &public
        ])
        .is_err());
    }

    #[test]
    #[ignore = "set FACULTIES_SECRETS_TEST_BIN to the separately built secrets binary"]
    fn native_import_cli_check_never_opens_destination_and_reports_bypass_perception() {
        use std::process::Command;
        let binary = std::env::var_os("FACULTIES_SECRETS_TEST_BIN").expect("explicit test binary");
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("synthetic.1pux");
        let pile = directory.path().join("synthetic.pile");
        let key = directory.path().join("synthetic.key");
        let bytes = super::super::onepassword::tests::fixture();
        std::fs::write(&source, &bytes).unwrap();
        let run = |check: bool| {
            let mut command = Command::new(&binary);
            command
                .arg("--pile")
                .arg(&pile)
                .arg("--key")
                .arg(&key)
                .arg("import-1password")
                .arg(&source)
                .env("DRIVE_ENDPOINT", "http://127.0.0.1:9");
            if check {
                command.arg("--check");
            }
            command.output().unwrap()
        };
        let checked = run(true);
        assert!(checked.status.success(), "bounded synthetic check failed");
        assert!(!pile.exists());
        assert!(!key.exists());
        assert!(String::from_utf8_lossy(&checked.stdout).contains("items 2"));
        std::fs::write(&pile, b"synthetic invalid pile sentinel").unwrap();
        std::fs::write(&key, b"synthetic invalid key sentinel").unwrap();
        let sentinels_checked = run(true);
        assert!(
            sentinels_checked.status.success(),
            "check must not inspect destination sentinels"
        );
        assert!(std::fs::read(&pile).unwrap() == b"synthetic invalid pile sentinel");
        assert!(std::fs::read(&key).unwrap() == b"synthetic invalid key sentinel");
        std::fs::remove_file(&key).unwrap();
        std::fs::File::create(&pile).unwrap();
        crate::storage::initialize_signer(&pile, Some(&key)).unwrap();
        let imported = run(false);
        let retry = run(false);
        for output in [&checked, &sentinels_checked, &imported, &retry] {
            assert!(output.status.success(), "synthetic importer process failed");
            for bytes in [&output.stdout, &output.stderr] {
                let text = String::from_utf8_lossy(bytes);
                assert!(!text.contains("SYNTHETIC-PRIVATE-"));
                assert!(!text.contains("Public synthetic title"));
                assert!(!text.contains(source.to_str().unwrap()));
            }
        }
        assert!(String::from_utf8_lossy(&retry.stdout).contains("existing archive"));
        assert_eq!(std::fs::read(&source).unwrap(), bytes);
        let operations = Secrets::new(pile, Some(key));
        assert_eq!(operations.list_items().unwrap().len(), 2);
        assert_eq!(operations.list().unwrap().len(), 2);
    }

    const PILE: &str = "/tmp/never-opened-secrets-cli-test.pile";
    const ID: &str = "01010101010101010101010101010101";

    #[test]
    fn structured_cli_requires_file_input_and_exact_body_for_child_export() {
        assert!(Cli::try_parse_from([
            "secrets",
            "--pile",
            PILE,
            "add-login",
            "--label",
            "public",
            "--password-file",
            "-"
        ])
        .is_ok());
        assert!(Cli::try_parse_from([
            "secrets",
            "--pile",
            PILE,
            "add-login",
            "--label",
            "public",
            "--password",
            "not-a-supported-literal"
        ])
        .is_err());
        assert!(
            Cli::try_parse_from(["secrets", "--pile", PILE, "item-open", "--secret", ID]).is_ok()
        );
        assert!(Cli::try_parse_from([
            "secrets",
            "--pile",
            PILE,
            "item-export",
            "--secret",
            ID,
            "--child",
            &"02".repeat(32)
        ])
        .is_err());
    }

    // The explicit path keeps this real CLI regression from making Cargo link
    // all faculty binaries merely to define CARGO_BIN_EXE_* for integration tests.
    #[test]
    #[ignore = "set FACULTIES_SECRETS_TEST_BIN to the separately built secrets binary"]
    fn structured_cli_binary_exports_exact_selected_values_and_keeps_receipts_public() {
        use faculties_secrets::{
            item::PrivateText,
            schema::{attachment_bytes, field_role, field_text, ROLE_PASSWORD},
        };
        use std::process::{Command, Stdio};
        use triblespace::prelude::*;
        let binary = std::env::var_os("FACULTIES_SECRETS_TEST_BIN").expect("explicit test binary");
        let dir = tempfile::tempdir().unwrap();
        let pile = dir.path().join("synthetic.pile");
        let key = dir.path().join("synthetic.key");
        let username = dir.path().join("username-input");
        let attachment = dir.path().join("SYNTHETIC-CLI-PRIVATE-filename.bin");
        std::fs::File::create(&pile).unwrap();
        crate::storage::initialize_signer(&pile, Some(&key)).unwrap();
        std::fs::write(&username, b"SYNTHETIC-CLI-PRIVATE-user").unwrap();
        std::fs::write(&attachment, b"SYNTHETIC-CLI-PRIVATE-binary\0\xff\n").unwrap();
        let command = || {
            let mut command = Command::new(&binary);
            command
                .arg("--pile")
                .arg(&pile)
                .arg("--key")
                .arg(&key)
                .env_remove("DRIVE_ENDPOINT");
            command
        };
        let mut child = command()
            .args([
                "add-login",
                "--label",
                "explicit-public-label",
                "--password-file",
                "-",
                "--username-file",
            ])
            .arg(&username)
            .arg("--attachment")
            .arg(&attachment)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"SYNTHETIC-CLI-PRIVATE-password\r\n")
            .unwrap();
        let added = child.wait_with_output().unwrap();
        assert!(
            added.status.success(),
            "{}",
            String::from_utf8_lossy(&added.stderr)
        );
        for bytes in [&added.stdout, &added.stderr] {
            assert!(!String::from_utf8_lossy(bytes).contains("SYNTHETIC-CLI-PRIVATE-"));
        }
        let receipt = String::from_utf8(added.stdout).unwrap();
        let id = parse_id(receipt.split_whitespace().nth(1).unwrap()).unwrap();
        let listed = command().arg("items").output().unwrap();
        assert!(listed.status.success());
        assert!(!String::from_utf8_lossy(&listed.stdout).contains("SYNTHETIC-CLI-PRIVATE-"));
        let operations = Secrets::new(pile.clone(), Some(key.clone()));
        let body_handle = operations.open_item(id).unwrap().remove(0);
        let facts = operations.open_item_body(id, body_handle).unwrap();
        let body = hex::encode(body_handle.raw);
        let password = find!(value: PrivateText, pattern!(&facts, [{field_role: ROLE_PASSWORD, field_text: ?value}])).next().unwrap();
        let binary_child =
            find!(value: Inline<inlineencodings::Handle<Encrypted<blobencodings::RawBytes>>>,
            pattern!(&facts, [{attachment_bytes: ?value}]))
            .next()
            .unwrap();
        let alternatives = command()
            .args(["item-open", "--secret", &format!("{id:x}")])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8(alternatives.stdout).unwrap(),
            format!("blake3:{body}\n")
        );
        let native = command()
            .args(["item-open", "--secret", &format!("{id:x}"), "--body", &body])
            .env("DRIVE_ENDPOINT", "invalid-and-must-not-be-used")
            .output()
            .unwrap();
        assert!(
            native.status.success(),
            "{}",
            String::from_utf8_lossy(&native.stderr)
        );
        let archive: Blob<blobencodings::SimpleArchive> = Blob::new(native.stdout.into());
        assert_eq!(archive.try_from_blob::<TribleSet>().unwrap(), facts);
        let inspected = command()
            .args([
                "item-inspect",
                "--secret",
                &format!("{id:x}"),
                "--body",
                &body,
            ])
            .env("DRIVE_ENDPOINT", "invalid-and-must-not-be-used")
            .output()
            .unwrap();
        assert!(
            inspected.status.success(),
            "{}",
            String::from_utf8_lossy(&inspected.stderr)
        );
        let inspection = String::from_utf8(inspected.stdout).unwrap();
        assert!(inspection.contains("password"));
        assert!(inspection.contains(&hex::encode(password.raw)));
        assert!(!inspection.contains("SYNTHETIC-CLI-PRIVATE-"));
        let exported = command()
            .args([
                "item-export",
                "--secret",
                &format!("{id:x}"),
                "--body",
                &body,
                "--child",
                &hex::encode(password.raw),
                "--text",
            ])
            .env("DRIVE_ENDPOINT", "invalid-and-must-not-be-used")
            .output()
            .unwrap();
        assert!(
            exported.status.success(),
            "{}",
            String::from_utf8_lossy(&exported.stderr)
        );
        assert_eq!(exported.stdout, b"SYNTHETIC-CLI-PRIVATE-password\r\n");
        let binary_export = command()
            .args([
                "item-export",
                "--secret",
                &format!("{id:x}"),
                "--body",
                &body,
                "--child",
                &hex::encode(binary_child.raw),
            ])
            .output()
            .unwrap();
        assert!(binary_export.status.success());
        assert_eq!(
            binary_export.stdout,
            b"SYNTHETIC-CLI-PRIVATE-binary\0\xff\n"
        );
        let wrong_type = command()
            .args([
                "item-export",
                "--secret",
                &format!("{id:x}"),
                "--body",
                &body,
                "--child",
                &hex::encode(password.raw),
            ])
            .output()
            .unwrap();
        assert!(!wrong_type.status.success());
        assert!(wrong_type.stdout.is_empty());
        assert!(!String::from_utf8_lossy(&wrong_type.stderr).contains("SYNTHETIC-CLI-PRIVATE-"));
        // Existing byte CLI remains exercisable, with file input rather than a
        // secret argv value. Structured versions are absent from its listing.
        let raw = command()
            .args(["add", "--name", "raw-public", "--value"])
            .arg(format!("@{}", username.display()))
            .output()
            .unwrap();
        assert!(raw.status.success());
        let raw_id = String::from_utf8(raw.stdout)
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .to_owned();
        let raw_export = command()
            .args(["get", "--secret", &raw_id])
            .output()
            .unwrap();
        assert!(raw_export.status.success());
        assert_eq!(raw_export.stdout, b"SYNTHETIC-CLI-PRIVATE-user");
    }

    #[test]
    fn resource_native_surface_keeps_add_get_and_selected_maintenance() {
        for args in [
            vec![
                "secrets", "--pile", PILE, "add", "--name", "token", "--value", "value",
            ],
            vec!["secrets", "--pile", PILE, "get", "--secret", ID],
            vec!["secrets", "--pile", PILE, "list"],
            vec!["secrets", "--pile", PILE, "maintain"],
        ] {
            assert!(Cli::try_parse_from(args).is_ok());
        }
        for removed in ["vault", "revoke", "identity", "scope"] {
            assert!(Cli::try_parse_from(["secrets", "--pile", PILE, removed]).is_err());
        }
    }

    #[test]
    fn grant_requires_exactly_one_resource_selection_and_a_recipient() {
        let recipient = hex::encode(
            ed25519_dalek::SigningKey::from_bytes(&[1; 32])
                .verifying_key()
                .to_bytes(),
        );
        let resource = "02".repeat(32);
        assert!(Cli::try_parse_from([
            "secrets",
            "--pile",
            PILE,
            "grant",
            "--secret",
            ID,
            "--recipient",
            &recipient,
            "--delegate",
            "--expires-at",
            "2030-01-01T00:00:00Z"
        ])
        .is_ok());
        assert!(Cli::try_parse_from([
            "secrets",
            "--pile",
            PILE,
            "grant",
            "--resource",
            &resource,
            "--recipient",
            &recipient
        ])
        .is_ok());
        assert!(Cli::try_parse_from([
            "secrets",
            "--pile",
            PILE,
            "grant",
            "--recipient",
            &recipient
        ])
        .is_err());
        assert!(Cli::try_parse_from([
            "secrets",
            "--pile",
            PILE,
            "grant",
            "--secret",
            ID,
            "--resource",
            &resource,
            "--recipient",
            &recipient
        ])
        .is_err());
    }
}
