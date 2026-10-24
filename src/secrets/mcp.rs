//! Explicit credential operations. Discovery/listing never opens plaintext.
use super::Secrets as Operations;
use crate::mcp::{decode_arguments, invalid_arguments, Faculty, Tool};
use crate::out::Out;
use anybytes::Bytes;
use anyhow::{bail, Result};
use base64::Engine as _;
use faculties_secrets::encrypted::Encrypted;
use faculties_secrets::resource::{DeliveryLimits, SecretTarget};
use serde::Deserialize;
use std::path::PathBuf;
use triblespace::prelude::{blobencodings, inlineencodings, Blob, Id, Inline, IntoBlob};
use zeroize::Zeroizing;

const EMPTY: &str = r#"{"type":"object","properties":{},"additionalProperties":false}"#;
const TOOLS: &[Tool] = &[
    Tool {name:"secrets_item_totp", description:"Explicitly generate a current TOTP from one selected encrypted item text field, identified by exact version, body and child ciphertext handles. Returns only a short-lived code, inclusive valid_from_unix/exclusive valid_until_unix and generated_at_unix in Unix seconds, period, digits, algorithm, and normalized issuer/account metadata (null for a bare Base32 seed). The seed and full URI remain inside Secrets. Supports SHA1/SHA256/SHA512, 6/8 digits, positive period; no HOTP, client-supplied time, enrollment or mutation.", input_schema:r#"{"type":"object","properties":{"secret":{"type":"string"},"body":{"type":"string"},"child":{"type":"string"}},"required":["secret","body","child"],"additionalProperties":false}"#},
    Tool {name:"secrets_item_inspect", description:"Explicit read-only inspection of one exact private item body: known role names with opaque IDs, typed ciphertext child handles and encrypted label/filename handles. Does not decrypt values, labels or filenames. Repeated/unknown roles remain visible.", input_schema:r#"{"type":"object","properties":{"secret":{"type":"string"},"body":{"type":"string"}},"required":["secret","body"],"additionalProperties":false}"#},
    Tool {name:"secrets_items", description:"List structured encrypted item version IDs and explicit public labels only. Never decrypts values, private labels or filenames.", input_schema:EMPTY},
    Tool {name:"secrets_item_open", description:"Explicitly open a structured item. Without body, disclose body alternatives (advertised ciphertext metadata without unlocking for user-protected versions; decryptable alternatives for ordinary versions). With exact body, export that private native SimpleArchive as a binary resource; its child values remain encrypted handles. User-protected opening requires one local Erlkonig unlock; never supply a password in arguments. Does not read server files.", input_schema:r#"{"type":"object","properties":{"secret":{"type":"string"},"body":{"type":"string"}},"required":["secret"],"additionalProperties":false}"#},
    Tool {name:"secrets_item_export", description:"Explicitly decrypt one selected item child as a binary resource. Requires exact version, body and child ciphertext handles and encoding text or bytes. Reveals plaintext to the caller, not perception. No server files or stdin.", input_schema:r#"{"type":"object","properties":{"secret":{"type":"string"},"body":{"type":"string"},"child":{"type":"string"},"encoding":{"type":"string","enum":["text","bytes"]}},"required":["secret","body","child","encoding"],"additionalProperties":false}"#},
    Tool {name:"secrets_add", description:"Encrypt one immutable version from literal value text or base64 bytes. Supply exactly one. Returns its ID, never echoes plaintext, and never reads a server file or stdin. Repeating an add creates a new version.", input_schema:r#"{"type":"object","properties":{"name":{"type":"string"},"value":{"type":"string"},"data_base64":{"type":"string"}},"required":["name"],"oneOf":[{"required":["value"]},{"required":["data_base64"]}],"additionalProperties":false}"#},
    Tool {name:"secrets_get", description:"Explicitly decrypt one exact immutable version and return its original plaintext bytes as an embedded binary resource. This reveals the selected secret to the caller; it is not a metadata query or image/audio perception. A user-protected version requires one local Erlkonig unlock; never supply a password in arguments, and do not automatically retry a denied or expired request.", input_schema:r#"{"type":"object","properties":{"secret":{"type":"string","description":"Exact nonzero 32-digit version ID"}},"required":["secret"],"additionalProperties":false}"#},
    Tool {name:"secrets_list", description:"List immutable secret version IDs and names in the configured collection. Does not decrypt secret values.", input_schema:EMPTY},
    Tool {name:"secrets_grant", description:"Grant future DEK delivery for one exact secret or resource. Does not grant collection READ/WRITE or reveal plaintext. Optional delivery deadlines do not expire already-delivered envelopes; delegate permits onward grants.", input_schema:r#"{"type":"object","properties":{"secret":{"type":"string"},"resource":{"type":"string"},"recipient":{"type":"string"},"not_before":{"type":"string","format":"date-time"},"expires_at":{"type":"string","format":"date-time"},"delegate":{"type":"boolean","default":false}},"required":["recipient"],"oneOf":[{"required":["secret"]},{"required":["resource"]}],"additionalProperties":false}"#},
    Tool {name:"secrets_maintain", description:"Deliver existing encrypted DEKs to authorized resource-specific recipients. Optional secret/resource selections limit the pass; otherwise visits all bound resources the holder can open. No grants, body rewrites, or plaintext output.", input_schema:r#"{"type":"object","properties":{"secrets":{"type":"array","items":{"type":"string"},"default":[]},"resources":{"type":"array","items":{"type":"string"},"default":[]}},"additionalProperties":false}"#},
];
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Add {
    name: String,
    value: Option<String>,
    data_base64: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Get {
    secret: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ItemOpen {
    secret: String,
    body: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ItemInspect {
    secret: String,
    body: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ItemExport {
    secret: String,
    body: String,
    child: String,
    encoding: ChildEncoding,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ItemTotp {
    secret: String,
    body: String,
    child: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum ChildEncoding {
    Text,
    Bytes,
}

fn item_id(raw: &str) -> Result<Id> {
    Id::from_hex(raw.trim())
        .ok_or_else(|| invalid_arguments("secret must be an exact nonzero 32-digit version ID"))
}
fn ciphertext_handle(raw: &str) -> Result<[u8; 32]> {
    hex::decode(raw.strip_prefix("blake3:").unwrap_or(raw))
        .map_err(|_| invalid_arguments("ciphertext handle must be 64 hexadecimal digits"))?
        .try_into()
        .map_err(|_| invalid_arguments("ciphertext handle must be 64 hexadecimal digits"))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Grant {
    secret: Option<String>,
    resource: Option<String>,
    recipient: String,
    not_before: Option<String>,
    expires_at: Option<String>,
    #[serde(default)]
    delegate: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Maintain {
    #[serde(default)]
    secrets: Vec<String>,
    #[serde(default)]
    resources: Vec<String>,
}

fn secret_target(raw: &str) -> Result<SecretTarget> {
    Id::from_hex(raw.trim())
        .map(SecretTarget::Secret)
        .ok_or_else(|| invalid_arguments("secret must be an exact nonzero 32-digit version ID"))
}

fn resource_target(raw: &str) -> Result<SecretTarget> {
    super::cli::parse_resource(raw)
        .map(SecretTarget::Resource)
        .map_err(invalid_arguments)
}

pub struct Secrets {
    operations: Operations,
}
impl Secrets {
    pub fn new(pile: PathBuf, key: Option<PathBuf>) -> Self {
        Self::with_storage(crate::storage::Storage::new(pile, key))
    }
    pub fn with_storage(storage: crate::storage::Storage) -> Self {
        Self {
            operations: Operations::with_storage(storage),
        }
    }
}
impl Faculty for Secrets {
    fn tools(&self) -> &[Tool] {
        TOOLS
    }
    fn call(&self, name: &str, arguments: Bytes, out: &mut Out<'_>) -> Result<()> {
        match name {
            "secrets_item_totp" => {
                let args: ItemTotp = decode_arguments(arguments)?;
                let code = self.operations.item_totp(
                    item_id(&args.secret)?,
                    Inline::new(ciphertext_handle(&args.body)?),
                    Inline::new(ciphertext_handle(&args.child)?),
                )?;
                out.line(super::operations::totp_json(&code)?)
            }
            "secrets_item_inspect" => {
                let args: ItemInspect = decode_arguments(arguments)?;
                let id = item_id(&args.secret)?;
                let body = ciphertext_handle(&args.body)?;
                self.operations.inspect_item(id, Inline::new(body), out)
            }
            "secrets_items" => {
                let _: Empty = decode_arguments(arguments)?;
                let rows = self.operations.list_items()?;
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
            "secrets_item_open" => {
                let args: ItemOpen = decode_arguments(arguments)?;
                let id = item_id(&args.secret)?;
                if let Some(body) = args.body {
                    let body = ciphertext_handle(&body)?;
                    let facts = self.operations.open_item_body(id, Inline::new(body))?;
                    let blob: Blob<blobencodings::SimpleArchive> = facts.to_blob();
                    out.blob(
                        blob.bytes,
                        "application/octet-stream",
                        format!("secrets://{id:x}/{}/facts", hex::encode(body)),
                    )
                } else {
                    for body in self.operations.open_item(id)? {
                        out.line(format!("blake3:{}", hex::encode(body.raw)))?;
                    }
                    Ok(())
                }
            }
            "secrets_item_export" => {
                let args: ItemExport = decode_arguments(arguments)?;
                let id = item_id(&args.secret)?;
                let body = ciphertext_handle(&args.body)?;
                let child = ciphertext_handle(&args.child)?;
                let bytes =
                    match args.encoding {
                        ChildEncoding::Text => {
                            self.operations
                                .open_item_child(
                                    id,
                                    Inline::new(body),
                                    Inline::<
                                        inlineencodings::Handle<
                                            Encrypted<blobencodings::UTF8String>,
                                        >,
                                    >::new(child),
                                )?
                                .bytes
                        }
                        ChildEncoding::Bytes => {
                            self.operations
                                .open_item_child(
                                    id,
                                    Inline::new(body),
                                    Inline::<
                                        inlineencodings::Handle<Encrypted<blobencodings::RawBytes>>,
                                    >::new(child),
                                )?
                                .bytes
                        }
                    };
                out.blob(
                    bytes,
                    "application/octet-stream",
                    format!(
                        "secrets://{id:x}/{}/{}",
                        hex::encode(body),
                        hex::encode(child)
                    ),
                )
            }
            "secrets_add" => {
                let args: Add = decode_arguments(arguments)?;
                let plaintext = match (args.value, args.data_base64) {
                    (Some(value), None) => Zeroizing::new(value.into_bytes()),
                    (None, Some(encoded)) => {
                        let encoded = Zeroizing::new(encoded);
                        Zeroizing::new(
                            base64::engine::general_purpose::STANDARD
                                .decode(encoded.as_bytes())
                                .map_err(|_| invalid_arguments("invalid data_base64"))?,
                        )
                    }
                    _ => {
                        return Err(invalid_arguments(
                            "supply exactly one of value or data_base64",
                        ))
                    }
                };
                let id = self.operations.add(&args.name, &plaintext)?;
                out.line(format!("secret {id:x}  {}", args.name))
            }
            "secrets_get" => {
                let args: Get = decode_arguments(arguments)?;
                let secret = Id::from_hex(args.secret.trim()).ok_or_else(|| {
                    invalid_arguments("secret must be an exact nonzero 32-digit version ID")
                })?;
                let plaintext = self.operations.get(secret)?;
                out.blob(
                    plaintext.to_vec(),
                    "application/octet-stream",
                    format!("secrets://{secret:x}"),
                )
            }
            "secrets_list" => {
                let _: Empty = decode_arguments(arguments)?;
                let rows = self.operations.list()?;
                if rows.is_empty() {
                    out.line("(no secrets)")?;
                }
                for row in rows {
                    out.line(format!("{:x}  {}", row.id, row.name))?;
                }
                Ok(())
            }
            "secrets_maintain" => {
                let args: Maintain = decode_arguments(arguments)?;
                let selected = args
                    .secrets
                    .iter()
                    .map(|raw| secret_target(raw))
                    .chain(args.resources.iter().map(|raw| resource_target(raw)))
                    .collect::<Result<Vec<_>>>()?;
                out.line(format!(
                    "added {} recipient envelope(s)",
                    self.operations.maintain_selected(&selected)?
                ))
            }
            "secrets_grant" => {
                let args: Grant = decode_arguments(arguments)?;
                let target = match (args.secret, args.resource) {
                    (Some(secret), None) => secret_target(&secret)?,
                    (None, Some(resource)) => resource_target(&resource)?,
                    _ => {
                        return Err(invalid_arguments(
                            "supply exactly one of secret or resource",
                        ))
                    }
                };
                let recipient =
                    super::cli::parse_recipient(&args.recipient).map_err(invalid_arguments)?;
                let parse_time = |raw: String| {
                    raw.parse()
                        .map_err(|_| invalid_arguments("invalid delivery timestamp"))
                };
                let limits = DeliveryLimits {
                    not_before: args.not_before.map(parse_time).transpose()?,
                    expires_at: args.expires_at.map(parse_time).transpose()?,
                };
                for id in self
                    .operations
                    .grant(target, recipient, limits, args.delegate)?
                {
                    out.line(format!("AUTH blake3:{}", hex::encode(id.raw)))?;
                }
                Ok(())
            }
            _ => bail!("Secrets MCP has no tool {name:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::out::Part;
    use faculties_secrets::item::{FieldInput, FieldValue, ItemInput, PrivateText};
    use faculties_secrets::schema::{field_role, field_text, ROLE_PASSWORD};
    use triblespace::prelude::*;

    fn call(adapter: &Secrets, name: &str, args: serde_json::Value) -> Result<Vec<Part>> {
        let mut parts = Vec::new();
        adapter.call(
            name,
            serde_json::to_vec(&args)?.into(),
            &mut Out::new(&mut |part| {
                parts.push(part);
                Ok(())
            }),
        )?;
        Ok(parts)
    }

    fn totp_fixture() -> (
        tempfile::TempDir,
        PathBuf,
        PathBuf,
        Secrets,
        serde_json::Value,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let pile = directory.path().join("totp.pile");
        let key = directory.path().join("totp.key");
        std::fs::File::create(&pile).unwrap();
        crate::storage::initialize_signer(&pile, Some(&key)).unwrap();
        let operations = Operations::new(pile.clone(), Some(key.clone()));
        let id = operations.add_item("public synthetic TOTP", ItemInput {
            fields: &[
                FieldInput { role: faculties_secrets::schema::ROLE_CUSTOM, label: Some("SYNTHETIC-PRIVATE-LABEL"),
                    value: FieldValue::Text("otpauth://totp/Example:alice?secret=MY&issuer=Example&period=45&digits=8&algorithm=SHA512") },
                FieldInput { role: faculties_secrets::schema::ROLE_CUSTOM, label: None,
                    value: FieldValue::Text("SYNTHETIC-PRIVATE-OTHER") },
            ], attachments: &[],
        }).unwrap();
        let bodies = operations.open_item(id).unwrap();
        assert_eq!(bodies.len(), 1);
        let body = bodies[0];
        let facts = operations.open_item_body(id, body).unwrap();
        let children = find!(child: PrivateText, pattern!(&facts, [{field_text: ?child}]));
        let child = children
            .into_iter()
            .find(|child| operations.item_totp(id, body, *child).is_ok())
            .unwrap();
        let arguments = serde_json::json!({"secret":format!("{id:x}"), "body":hex::encode(body.raw),
            "child":hex::encode(child.raw)});
        let adapter = Secrets::new(pile.clone(), Some(key.clone()));
        (directory, pile, key, adapter, arguments)
    }

    fn current_totp(adapter: &Secrets, args: serde_json::Value) -> serde_json::Value {
        let parts = call(adapter, "secrets_item_totp", args).unwrap();
        let [Part::Text { text }] = parts.as_slice() else {
            panic!("one JSON text result expected");
        };
        assert!(!text.contains("otpauth:"));
        assert!(!text.contains("secret="));
        assert!(!text.contains("SYNTHETIC-PRIVATE"));
        let value: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_totp(&value);
        value
    }

    fn assert_totp(value: &serde_json::Value) {
        assert_eq!(value.as_object().unwrap().len(), 9);
        assert_eq!(value["issuer"], "Example");
        assert_eq!(value["account"], "alice");
        assert_eq!(value["algorithm"], "SHA512");
        assert_eq!(value["digits"], 8);
        assert_eq!(value["period"], 45);
        let generated = value["generated_at_unix"].as_u64().unwrap();
        assert_eq!(
            value["valid_from_unix"].as_u64().unwrap(),
            generated / 45 * 45
        );
        assert_eq!(
            value["valid_until_unix"].as_u64().unwrap(),
            generated / 45 * 45 + 45
        );
        let code = value["code"].as_str().unwrap();
        assert_eq!(code.len(), 8);
        assert!(code.bytes().all(|byte| byte.is_ascii_digit()));
    }

    #[test]
    fn totp_metadata_bounds_and_exact_selection_errors_are_native() {
        let (_directory, pile, _key, adapter, args) = totp_fixture();
        current_totp(&adapter, args.clone());
        let before = std::fs::metadata(&pile).unwrap().len();
        for name in ["secret", "body", "child"] {
            let mut invalid = args.clone();
            invalid[name] = if name == "secret" {
                "01".repeat(16)
            } else {
                "00".repeat(32)
            }
            .into();
            assert!(adapter
                .call(
                    "secrets_item_totp",
                    serde_json::to_vec(&invalid).unwrap().into(),
                    &mut Out::new(&mut |_| panic!("failed selection emitted output"))
                )
                .is_err());
        }
        assert_eq!(std::fs::metadata(&pile).unwrap().len(), before);
    }

    #[test]
    fn totp_invalid_arguments_do_not_open_storage() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("absent.pile");
        let adapter = Secrets::new(path.clone(), None);
        for args in [
            r#"{"secret":"bad","body":"bad","child":"bad"}"#,
            r#"{"secret":"01010101010101010101010101010101","body":"00","child":"00"}"#,
            r#"{"secret":"01010101010101010101010101010101","body":"00","child":"00","unix_seconds":59}"#,
            r#"{"secret":"01010101010101010101010101010101","secret":"02020202020202020202020202020202","body":"00","child":"00"}"#,
        ] {
            let error = adapter
                .call(
                    "secrets_item_totp",
                    args.to_owned().into(),
                    &mut Out::new(&mut |_| panic!("invalid arguments emitted output")),
                )
                .unwrap_err();
            assert!(error
                .downcast_ref::<crate::mcp::InvalidArguments>()
                .is_some());
            assert!(!path.exists());
        }
    }

    #[test]
    #[ignore = "manual: requires SECRETS_TEST_BIN pointing to this source's explicitly built secrets binary"]
    fn totp_cli_mcp_subprocess_parity() {
        let binary = std::env::var_os("SECRETS_TEST_BIN")
            .expect("SECRETS_TEST_BIN is required; do not silently skip");
        let (_directory, pile, key, adapter, args) = totp_fixture();
        let mcp = current_totp(&adapter, args.clone());
        let output = std::process::Command::new(binary)
            .arg("--pile")
            .arg(pile)
            .arg("--key")
            .arg(key)
            .args([
                "item-totp",
                "--secret",
                args["secret"].as_str().unwrap(),
                "--body",
                args["body"].as_str().unwrap(),
                "--child",
                args["child"].as_str().unwrap(),
            ])
            .env("DRIVE_ENDPOINT", "invalid-and-must-not-be-used")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let cli: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_totp(&cli);
        if cli["valid_from_unix"] == mcp["valid_from_unix"] {
            assert_eq!(cli["code"], mcp["code"]);
        }
    }

    #[test]
    fn structured_discovery_is_public_and_explicit_exports_are_native_resources() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp-items.pile");
        std::fs::File::create(&path).unwrap();
        crate::storage::initialize_signer(&path, None).unwrap();
        let operations = Operations::new(path.clone(), None);
        let opaque_role = genid().id;
        let id = operations
            .add_item(
                "public MCP label",
                ItemInput {
                    fields: &[
                        FieldInput {
                            role: ROLE_PASSWORD,
                            label: None,
                            value: FieldValue::Text("SYNTHETIC-MCP-PRIVATE"),
                        },
                        FieldInput {
                            role: ROLE_PASSWORD,
                            label: None,
                            value: FieldValue::Text("SYNTHETIC-MCP-PRIVATE"),
                        },
                        FieldInput {
                            role: opaque_role,
                            label: Some("SYNTHETIC-MCP-PRIVATE-LABEL"),
                            value: FieldValue::Text("SYNTHETIC-MCP-PRIVATE-UNKNOWN"),
                        },
                    ],
                    attachments: &[],
                },
            )
            .unwrap();
        let body_handle = operations.open_item(id).unwrap().remove(0);
        let private = operations.open_item_body(id, body_handle).unwrap();
        let body = hex::encode(body_handle.raw);
        let child = find!(value: PrivateText, pattern!(&private, [{field_role: ROLE_PASSWORD, field_text: ?value}]))
            .next()
            .unwrap();
        let adapter = Secrets::new(path, None);
        let listed = call(&adapter, "secrets_items", serde_json::json!({})).unwrap();
        assert!(listed.iter().all(
            |part| matches!(part, Part::Text {text} if !text.contains("SYNTHETIC-MCP-PRIVATE"))
        ));
        let alternatives = call(
            &adapter,
            "secrets_item_open",
            serde_json::json!({"secret":format!("{id:x}")}),
        )
        .unwrap();
        assert!(matches!(&alternatives[0], Part::Text {text} if text.contains(&body)));
        let facts = call(
            &adapter,
            "secrets_item_open",
            serde_json::json!({"secret":format!("{id:x}"),"body":body}),
        )
        .unwrap();
        let Part::Blob { bytes, .. } = &facts[0] else {
            panic!("native resource expected");
        };
        let archive: Blob<blobencodings::SimpleArchive> = Blob::new(bytes.clone());
        assert_eq!(archive.try_from_blob::<TribleSet>().unwrap(), private);
        let inspected = call(
            &adapter,
            "secrets_item_inspect",
            serde_json::json!({"secret":format!("{id:x}"), "body":body}),
        )
        .unwrap();
        assert!(inspected
            .iter()
            .any(|part| matches!(part, Part::Text {text} if text.contains("password"))));
        assert_eq!(
            inspected
                .iter()
                .filter(|part| matches!(part, Part::Text {text} if text.contains("password")))
                .count(),
            2
        );
        assert!(inspected.iter().any(|part| matches!(part, Part::Text {text} if text.contains(&format!("role {opaque_role:x}")))));
        assert!(inspected.iter().any(
            |part| matches!(part, Part::Text {text} if text.contains(&hex::encode(child.raw)))
        ));
        assert!(inspected.iter().all(
            |part| matches!(part, Part::Text {text} if !text.contains("SYNTHETIC-MCP-PRIVATE"))
        ));
        let exported = call(
            &adapter,
            "secrets_item_export",
            serde_json::json!({"secret":format!("{id:x}"),
            "body":body,"child":hex::encode(child.raw),"encoding":"text"}),
        )
        .unwrap();
        assert!(
            matches!(&exported[0], Part::Blob {bytes, mime_type, ..} if bytes.as_ref() == b"SYNTHETIC-MCP-PRIVATE" && mime_type == "application/octet-stream")
        );
        assert!(call(
            &adapter,
            "secrets_item_export",
            serde_json::json!({"secret":format!("{id:x}"),
            "body":body,"child":hex::encode(child.raw),"encoding":"bytes"})
        )
        .is_err());
    }
}
