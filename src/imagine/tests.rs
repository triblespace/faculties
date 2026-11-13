//! Generation with provenance, end to end against a fake OpenAI server and
//! temporary piles. No real OpenAI call and no live pile anywhere.

use super::auth::{Provider, Selector};
use super::openai::fake::{self, Reply, Server};
use super::openai::{self, Endpoint, OutputFormat, Quality, Size};
use super::{Imagine, Request};
use crate::out::{Out, Part};
use crate::schemas::imagine::{imagine, KIND_GENERATION, PROVIDER_OPENAI, QUALITY_MEDIUM};
use crate::storage::Storage;
use anyhow::{Context, Result};
use std::path::PathBuf;
use triblespace::core::metadata;
use triblespace::core::repo::pile::PileSnapshot;
use triblespace::prelude::blobencodings::{RawBytes, UTF8String};
use triblespace::prelude::inlineencodings::{Handle, NsTAIInterval, U256BE};
use triblespace::prelude::*;

const KEY: &str = "sk-proj-SYNTHETIC-imagine-test-key-0123456789";
const FILES: Id = crate::schemas::files::DEFAULT_SCOPE_ID;
const IMAGINE: Id = crate::schemas::imagine::DEFAULT_SCOPE_ID;

type TextHandle = Inline<Handle<UTF8String>>;

fn fresh() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let pile = directory.path().join("imagine.pile");
    let key = directory.path().join("imagine.key");
    std::fs::File::create(&pile).unwrap();
    crate::storage::initialize_signer(&pile, Some(&key)).unwrap();
    (directory, pile, key)
}

/// A storage with `KEY` stored as a raw Secrets version and bound for OpenAI,
/// and an Imagine pointed at `server`.
fn bound(storage: &Storage, server: &Server) -> Imagine {
    let secret = crate::secrets::Secrets::with_storage(storage.clone())
        .add("openai", KEY.as_bytes())
        .unwrap();
    let imagine =
        Imagine::with_storage(storage.clone()).with_openai_endpoint(Endpoint::new(&server.base));
    imagine
        .auth_set(Provider::OpenAi, Selector::Version(secret))
        .unwrap();
    imagine
}

/// COMMIT records targeting the selected collection for `scope`.
fn commits_to(storage: &Storage, scope: Id) -> usize {
    storage
        .with_pile(|pile, signer| {
            let collection = storage.open_collection_local(pile, scope, signer.verifying_key())?;
            let snapshot = pile.snapshot().context("freeze test snapshot")?;
            let records = crate::storage::discovered_records(&snapshot)?;
            Ok(records
                .commits()
                .iter()
                .filter(|commit| commit.collection() == collection.handle())
                .count())
        })
        .unwrap()
}

/// The selected Files collection's facts and a reader for their blobs.
fn files_view(storage: &Storage) -> (TribleSet, PileSnapshot) {
    storage
        .with_pile(|pile, signer| {
            let collection = storage.open_collection_local(pile, FILES, signer.verifying_key())?;
            let snapshot = pile.snapshot().context("freeze test snapshot")?;
            let (facts, _) = crate::storage::read_fact_collection(collection, &snapshot)?;
            Ok((facts, snapshot))
        })
        .unwrap()
}

fn text(reader: &PileSnapshot, handle: TextHandle) -> String {
    let text: View<str> = reader.get(handle).unwrap();
    text.to_string()
}

fn number(value: Inline<U256BE>) -> u64 {
    value.try_from_inline::<u64>().unwrap()
}

/// Check that the Files collection holds `file` with exactly `bytes` and
/// `media_type`, and one generation of it with `prompt`; return that
/// generation's facts for further checks.
fn assert_stored(
    storage: &Storage,
    file: Id,
    generation: Id,
    bytes: &[u8],
    media_type: &str,
    prompt: &str,
) -> (TribleSet, PileSnapshot) {
    let (facts, reader) = files_view(storage);
    let resolved = crate::files::resolve_file(&facts, &reader, &format!("{file:x}")).unwrap();
    let stored: anybytes::Bytes = reader
        .get::<anybytes::Bytes, RawBytes>(resolved.content)
        .unwrap();
    assert_eq!(
        stored.as_ref(),
        bytes,
        "stored bytes are the decoded bytes, exactly"
    );
    let media_types: Vec<String> = crate::files::media_type_name_handles(&facts, file)
        .into_iter()
        .map(|handle| text(&reader, handle))
        .collect();
    assert_eq!(media_types, [media_type]);
    let generations: Vec<(Id, TextHandle)> = find!(
        (generation: Id, prompt: TextHandle),
        pattern!(&facts, [{ ?generation @
            metadata::tag: KIND_GENERATION,
            imagine::output: file,
            imagine::prompt: ?prompt,
        }])
    )
    .collect();
    assert_eq!(generations.len(), 1, "one generation of the file");
    assert_eq!(generations[0].0, generation);
    assert_eq!(text(&reader, generations[0].1), prompt);
    assert!(exists!(
        (at: Inline<NsTAIInterval>),
        pattern!(&facts, [{ generation @ metadata::created_at: ?at }])
    ));
    (facts, reader)
}

fn openai_request(prompt: &str) -> openai::Options {
    let mut options = openai::Options::new(prompt);
    options.quality = Quality::Medium;
    options.size = Size::Exact {
        width: 1024,
        height: 1024,
    };
    options
}

#[test]
fn a_generation_is_one_files_commit_with_exact_bytes_and_provenance() {
    let (_directory, pile, key) = fresh();
    let storage = Storage::new(pile, Some(key));
    let png = fake::image(image::ImageFormat::Png);
    let server = Server::start(Reply::Json(200, fake::success(&png, "png")));
    let imagine = bound(&storage, &server);
    assert_eq!(commits_to(&storage, FILES), 0);
    let bindings = commits_to(&storage, IMAGINE);

    let receipt = imagine
        .generate(&Request::OpenAi(openai_request("a lighthouse at dusk")))
        .unwrap();

    assert_eq!(server.count(), 1);
    assert_eq!(
        commits_to(&storage, FILES),
        1,
        "file and provenance in one commit"
    );
    assert_eq!(
        commits_to(&storage, IMAGINE),
        bindings,
        "generation writes no binding"
    );
    assert_eq!(receipt.generated.bytes().as_ref(), png.as_slice());
    let (facts, reader) = assert_stored(
        &storage,
        receipt.file,
        receipt.generation,
        &png,
        "image/png",
        "a lighthouse at dusk",
    );
    let generation = receipt.generation;
    let (provider, model, quality, width, height, total, created) = find!(
        (provider: Id, model: TextHandle, quality: Id, width: Inline<U256BE>, height: Inline<U256BE>,
            total: Inline<U256BE>, created: Inline<NsTAIInterval>),
        pattern!(&facts, [{ generation @
            imagine::provider: ?provider,
            imagine::model: ?model,
            imagine::quality: ?quality,
            imagine::width: ?width,
            imagine::height: ?height,
            imagine::total_tokens: ?total,
            imagine::response_created_at: ?created,
        }])
    )
    .next()
    .expect("the generation's request options and answer metadata");
    assert_eq!(provider, PROVIDER_OPENAI);
    assert_eq!(text(&reader, model), openai::DEFAULT_MODEL);
    assert_eq!(quality, QUALITY_MEDIUM);
    assert_eq!((number(width), number(height)), (1024, 1024));
    assert_eq!(number(total), 1106);
    let _ = created;
    assert!(exists!(
        (format: Id),
        pattern!(&facts, [{ generation @ imagine::output_format: ?format }])
    ));
    // The request carried the bound key, and nothing else carries it.
    assert!(server.heads()[0].contains(&format!("bearer {}", KEY.to_ascii_lowercase())));
}

#[test]
fn jpeg_keeps_its_bytes_and_media_type() {
    let (_directory, pile, key) = fresh();
    let storage = Storage::new(pile, Some(key));
    let jpeg = fake::image(image::ImageFormat::Jpeg);
    let server = Server::start(Reply::Json(200, fake::success(&jpeg, "jpeg")));
    let imagine = bound(&storage, &server);
    let mut options = openai_request("a cat");
    options.output_format = OutputFormat::Jpeg;
    options.output_compression = Some(80);
    let receipt = imagine.generate(&Request::OpenAi(options)).unwrap();
    assert_stored(
        &storage,
        receipt.file,
        receipt.generation,
        &jpeg,
        "image/jpeg",
        "a cat",
    );
    assert_eq!(commits_to(&storage, FILES), 1);
}

#[test]
fn a_provider_error_writes_nothing_and_says_what_openai_said() {
    let (_directory, pile, key) = fresh();
    let storage = Storage::new(pile, Some(key));
    let body =
        serde_json::json!({"error": {"message": "Your request was rejected by the safety system.",
        "type": "image_generation_user_error", "code": "moderation_blocked"}})
        .to_string();
    let server = Server::start(Reply::Json(400, body));
    let imagine = bound(&storage, &server);
    let error = imagine
        .generate(&Request::OpenAi(openai_request("x")))
        .unwrap_err();
    let text = format!("{error:#}");
    assert!(text.contains("rejected by the safety system"), "{text}");
    assert!(text.contains("moderation_blocked"), "{text}");
    assert!(!text.contains(KEY), "{text}");
    assert_eq!(server.count(), 1);
    assert_eq!(commits_to(&storage, FILES), 0);
}

#[test]
fn a_dropped_send_is_uncertain_writes_nothing_and_is_not_retried() {
    let (_directory, pile, key) = fresh();
    let storage = Storage::new(pile, Some(key));
    let server = Server::start(Reply::Drop);
    let imagine = bound(&storage, &server);
    let error = imagine
        .generate(&Request::OpenAi(openai_request("x")))
        .unwrap_err();
    let text = format!("{error:#}");
    assert!(text.contains("uncertain"), "{text}");
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert_eq!(server.count(), 1, "never retried");
    assert_eq!(commits_to(&storage, FILES), 0);
}

#[test]
fn a_timed_out_send_is_uncertain_writes_nothing_and_is_not_retried() {
    let (_directory, pile, key) = fresh();
    let storage = Storage::new(pile, Some(key));
    let server = Server::start(Reply::Stall(std::time::Duration::from_secs(5)));
    let imagine = bound(&storage, &server).with_openai_endpoint(
        Endpoint::new(&server.base).with_timeout(std::time::Duration::from_millis(500)),
    );
    let error = imagine
        .generate(&Request::OpenAi(openai_request("x")))
        .unwrap_err();
    assert!(format!("{error:#}").contains("uncertain"), "{error:#}");
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert_eq!(server.count(), 1, "never retried");
    assert_eq!(commits_to(&storage, FILES), 0);
}

#[test]
fn nothing_is_sent_without_a_bound_credential() {
    let (_directory, pile, key) = fresh();
    let storage = Storage::new(pile, Some(key));
    let server = Server::start(Reply::Json(200, fake::success(b"unused", "png")));
    let imagine =
        Imagine::with_storage(storage.clone()).with_openai_endpoint(Endpoint::new(&server.base));
    let error = imagine
        .generate(&Request::OpenAi(openai_request("x")))
        .unwrap_err();
    assert!(
        error.to_string().contains("imagine auth set openai"),
        "{error:#}"
    );
    assert_eq!(server.count(), 0);
    assert_eq!(commits_to(&storage, FILES), 0);
}

#[test]
fn auth_set_refuses_a_version_this_key_cannot_open_and_never_shows_the_key() {
    let (_directory, pile, key) = fresh();
    let storage = Storage::new(pile, Some(key));
    let stranger = ed25519_dalek::SigningKey::from_bytes(&[0x73; 32]);
    let sealed_for_stranger = storage
        .with_store(|store, signer, runtime| {
            let collection = crate::storage::open_secrets_collection_acquiring(
                store,
                signer.verifying_key(),
                runtime,
            )?;
            let sealed = crate::secrets::seal_version(
                "openai",
                KEY.as_bytes(),
                [stranger.verifying_key()],
                crate::clock::point_now()?,
            )?;
            store.commit(collection.source(), signer, sealed.fragment)?;
            Ok(sealed.secret)
        })
        .unwrap();
    let imagine = Imagine::with_storage(storage.clone());
    let bindings = commits_to(&storage, IMAGINE);
    for secret in [sealed_for_stranger, genid().id] {
        let error = imagine
            .auth_set(Provider::OpenAi, Selector::Version(secret))
            .unwrap_err();
        assert!(!format!("{error:#}").contains(KEY), "{error:#}");
        assert!(!format!("{error:?}").contains(KEY));
    }
    assert_eq!(commits_to(&storage, IMAGINE), bindings, "nothing was bound");
    assert!(imagine
        .auth_status(Provider::OpenAi)
        .unwrap()
        .heads
        .is_empty());

    // A version that opens but is not a usable key is refused the same way.
    let unusable = crate::secrets::Secrets::with_storage(storage.clone())
        .add("openai", format!("{KEY}\nsecond line").as_bytes())
        .unwrap();
    let error = imagine
        .auth_set(Provider::OpenAi, Selector::Version(unusable))
        .unwrap_err();
    assert!(!format!("{error:#}").contains(KEY), "{error:#}");
}

#[test]
fn an_item_field_credential_binds_by_exact_ids_and_is_used() {
    use crate::secrets::item::{FieldInput, FieldValue, ItemInput, PrivateText};
    use crate::secrets::schema::{field_text, item_field, KIND_ITEM, ROLE_CUSTOM, ROLE_URL};
    let (_directory, pile, key) = fresh();
    let storage = Storage::new(pile, Some(key));
    let secrets = crate::secrets::Secrets::with_storage(storage.clone());
    let item = secrets
        .add_item(
            "OPENAI_API_KEY",
            ItemInput {
                fields: &[
                    FieldInput {
                        role: ROLE_CUSTOM,
                        label: Some("credential"),
                        value: FieldValue::Text(KEY),
                    },
                    FieldInput {
                        role: ROLE_URL,
                        label: None,
                        value: FieldValue::Text("https://platform.openai.com"),
                    },
                ],
                attachments: &[],
            },
        )
        .unwrap();
    let body = secrets.open_item(item).unwrap().remove(0);
    let facts = secrets.open_item_body(item, body).unwrap();
    let fields: Vec<(Id, PrivateText)> = find!(
        (field: Id, child: PrivateText),
        pattern!(&facts, [
            { metadata::tag: KIND_ITEM, item_field: ?field },
            { ?field @ field_text: ?child }
        ])
    )
    .collect();
    let field = fields
        .iter()
        .find(|(_, child)| {
            secrets
                .open_item_child(item, body, *child)
                .unwrap()
                .bytes
                .as_ref()
                == KEY.as_bytes()
        })
        .expect("the credential field")
        .0;

    let png = fake::image(image::ImageFormat::Png);
    let server = Server::start(Reply::Json(200, fake::success(&png, "png")));
    let imagine =
        Imagine::with_storage(storage.clone()).with_openai_endpoint(Endpoint::new(&server.base));
    let missing = imagine
        .auth_set(
            Provider::OpenAi,
            Selector::ItemField {
                secret: item,
                body,
                field: genid().id,
            },
        )
        .unwrap_err();
    assert!(missing.to_string().contains("no text field"), "{missing:#}");
    let (_, credential) = imagine
        .auth_set(
            Provider::OpenAi,
            Selector::ItemField {
                secret: item,
                body,
                field,
            },
        )
        .unwrap();
    let described = credential.describe();
    assert!(described.contains(&format!("{field:x}")) && !described.contains(KEY));
    assert_eq!(
        imagine
            .auth_status(Provider::OpenAi)
            .unwrap()
            .selected(Provider::OpenAi)
            .unwrap(),
        credential
    );
    imagine
        .generate(&Request::OpenAi(openai_request("a fox")))
        .unwrap();
    assert!(server.heads()[0].contains(&format!("bearer {}", KEY.to_ascii_lowercase())));
}

/// Sol's finding: a workspace (routed) storage must run the OpenAI path
/// through its own Files and Secrets routes, not refuse it.
#[test]
fn a_routed_workspace_storage_runs_the_openai_path_through_its_routes() {
    let (_directory, pile, key) = fresh();
    let base = Storage::new(pile, Some(key));
    let routes = base
        .with_pile(|store, signer| {
            let policy = crate::collection_names::private_policy(signer.verifying_key());
            let mut routes = crate::storage::CollectionRoutes::new();
            for (scope, name) in [
                (FILES, "workspace files"),
                (crate::secrets::DEFAULT_SCOPE_ID, "workspace secrets"),
                (IMAGINE, "workspace imagine"),
            ] {
                routes.insert(scope, store.collection(name, policy.clone())?.handle());
            }
            Ok(routes)
        })
        .unwrap();
    let routed = base.clone().with_collections(routes.clone());
    let png = fake::image(image::ImageFormat::Png);
    let server = Server::start(Reply::Json(200, fake::success(&png, "png")));
    let imagine = bound(&routed, &server);
    let receipt = imagine
        .generate(&Request::OpenAi(openai_request("a routed tree")))
        .unwrap();
    assert_eq!(commits_to(&routed, FILES), 1);
    assert_stored(
        &routed,
        receipt.file,
        receipt.generation,
        &png,
        "image/png",
        "a routed tree",
    );
    // The signer-private default Files collection received nothing.
    assert_eq!(commits_to(&base, FILES), 0);

    // The MCP adapter keeps the routed storage too.
    let adapter = super::mcp::Imagine::from_operations(
        Imagine::with_storage(routed.clone()).with_openai_endpoint(Endpoint::new(&server.base)),
    );
    let parts = call(&adapter, serde_json::json!({"prompt": "again"})).unwrap();
    assert!(matches!(&parts[0], Part::Text { text } if text.starts_with("files:")));
    assert_eq!(commits_to(&routed, FILES), 2);

    // Only the local FLUX path keeps its model-asset gating.
    let flux = Request::Flux(super::FluxOptions {
        prompt: "x".into(),
        variant: super::Variant::Klein,
        steps: None,
        seed: 0,
        width: 1024,
        height: 1024,
        guidance: 4.0,
    });
    let error = imagine.generate(&flux).unwrap_err();
    assert!(
        error.to_string().contains("model-asset routing"),
        "{error:#}"
    );
    assert_eq!(commits_to(&routed, FILES), 2);
}

fn call(adapter: &super::mcp::Imagine, arguments: serde_json::Value) -> Result<Vec<Part>> {
    use crate::mcp::Faculty;
    let mut parts = Vec::new();
    adapter.call(
        "imagine_generate",
        serde_json::to_vec(&arguments)?.into(),
        &mut Out::new(&mut |part| {
            parts.push(part);
            Ok(())
        }),
    )?;
    Ok(parts)
}

#[test]
fn mcp_emits_the_files_receipt_first_then_the_exact_image() {
    let (_directory, pile, key) = fresh();
    let storage = Storage::new(pile, Some(key));
    let webp = fake::image(image::ImageFormat::WebP);
    let server = Server::start(Reply::Json(200, fake::success(&webp, "webp")));
    let operations = bound(&storage, &server);
    let adapter = super::mcp::Imagine::from_operations(operations);
    let parts = call(
        &adapter,
        serde_json::json!({"prompt": "a heron", "output_format": "webp", "quality": "low"}),
    )
    .unwrap();
    let Part::Text { text } = &parts[0] else {
        panic!("the receipt comes first: {parts:?}");
    };
    assert!(
        text.starts_with("files:") && text.contains("image/webp"),
        "{text}"
    );
    let Part::Image { bytes, mime_type } = &parts[1] else {
        panic!("then the image: {parts:?}");
    };
    assert_eq!(bytes.as_ref(), webp.as_slice());
    assert_eq!(mime_type, "image/webp");
    assert_eq!(server.bodies()[0]["quality"], "low");
    assert_eq!(commits_to(&storage, FILES), 1);
}

#[test]
fn mcp_omits_an_image_too_large_for_the_response_and_still_succeeds() {
    let (_directory, pile, key) = fresh();
    let storage = Storage::new(pile, Some(key));
    let mut huge = b"\x89PNG\r\n\x1a\n".to_vec();
    huge.resize(7 * 1024 * 1024, 0);
    let server = Server::start(Reply::Json(200, fake::success(&huge, "png")));
    let adapter = super::mcp::Imagine::from_operations(bound(&storage, &server));
    let parts = call(&adapter, serde_json::json!({"prompt": "a very large map"})).unwrap();
    assert!(matches!(&parts[0], Part::Text { text } if text.starts_with("files:")));
    assert!(
        parts.iter().all(|part| !matches!(part, Part::Image { .. })),
        "no image block"
    );
    assert!(parts
        .iter()
        .any(|part| matches!(part, Part::Text { text } if text.contains("image not attached"))));
    assert_eq!(server.count(), 1, "the provider is not called again");
    assert_eq!(
        commits_to(&storage, FILES),
        1,
        "the image is stored all the same"
    );
}

#[test]
fn mcp_refuses_options_of_the_other_provider_before_sending() {
    let (_directory, pile, key) = fresh();
    let storage = Storage::new(pile, Some(key));
    let server = Server::start(Reply::Json(200, fake::success(b"unused", "png")));
    let adapter = super::mcp::Imagine::from_operations(bound(&storage, &server));
    assert!(call(&adapter, serde_json::json!({"prompt": "x", "steps": 4})).is_err());
    assert!(call(
        &adapter,
        serde_json::json!({"prompt": "x", "provider": "flux", "quality": "high"})
    )
    .is_err());
    assert!(call(
        &adapter,
        serde_json::json!({"prompt": "x", "size": "100x100"})
    )
    .is_err());
    assert_eq!(server.count(), 0);
}

#[test]
fn the_tool_schema_carries_no_encoded_bytes() {
    use crate::mcp::Faculty;
    let adapter = super::mcp::Imagine::new(PathBuf::from("/nonexistent.pile"), None);
    let tools = adapter.tools();
    assert_eq!(tools.len(), 1);
    let schema: serde_json::Value = serde_json::from_str(tools[0].input_schema).unwrap();
    let properties = schema["properties"].as_object().unwrap();
    for name in properties.keys() {
        for forbidden in ["b64", "base64", "bytes", "data", "image", "url", "path"] {
            assert!(!name.contains(forbidden), "{name}");
        }
    }
    let text = format!("{}{}", tools[0].description, tools[0].input_schema).to_ascii_lowercase();
    assert!(!text.contains("base64") && !text.contains("b64_json"));
    assert!(properties.contains_key("provider") && properties.contains_key("quality"));
}
