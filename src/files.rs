//! Canonical file-record construction shared by every faculty.
//!
//! A file entity is an immutable description of one named file value. Its
//! intrinsic identity is derived from exactly four facts:
//!
//! - [`metadata::tag`] = [`KIND_FILE`]
//! - [`file::content`] = the content-addressed raw-bytes handle
//! - [`file::name`] = the leaf name (never a containing path)
//! - [`file::media_type`] = an intrinsic [`KIND_MEDIA_TYPE`] entity
//!
//! Paths, source-system identifiers, timestamps, tags, embeddings, and other
//! provenance are deliberately absent from that identity. New callers attach
//! those facts to imports or source-specific occurrence entities; historical
//! producers also attached path/timestamp provenance to file ids directly.
//!
//! Readers query at their point of use and read the facts as observations:
//! a file named twice has two names, an import observed at two times has two
//! times, and nothing here refuses a collection for holding either. Only a
//! value that cannot be decoded is skipped, and only a selector that names
//! several things is answered with the candidates instead of a winner.

pub mod cli;
pub mod mcp;
pub mod operations;
pub mod presentation;

pub use operations::Files;

use anyhow::{anyhow, Context, Result};
use ed25519_dalek::SigningKey;
use std::collections::BTreeSet;
use std::path::Path;
use triblespace::core::collection::{CollectionCommit, CollectionSnapshotExt, CollectionStoreExt};
use triblespace::core::metadata;
use triblespace::core::query::TriblePattern;
use triblespace::core::repo::pile::{Pile, PileSnapshot};
use triblespace::core::repo::BlobStoreGet;
use triblespace::prelude::blobencodings::{RawBytes, UTF8String};
use triblespace::prelude::inlineencodings::{Handle, NsTAIInterval};
use triblespace::prelude::*;

use crate::collection_names::open_configured;
use crate::schemas::files::{file, KIND_DIRECTORY, KIND_FILE, KIND_IMPORT, KIND_MEDIA_TYPE};

pub type ContentHandle = Inline<Handle<RawBytes>>;
pub type NameHandle = Inline<Handle<UTF8String>>;
pub type ImportTime = Inline<NsTAIInterval>;
pub const DEFAULT_MEDIA_TYPE: &str = "application/octet-stream";

/// The two canonical targets carried by a `files:` reference token.
///
/// Unlike an entity selector, a content reference identifies bytes directly:
/// several named file entities may intentionally share one content handle.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum FileReference {
    Entity(Id),
    Content(ContentHandle),
}

impl FileReference {
    pub fn hex(self) -> String {
        match self {
            Self::Entity(entity) => format!("{entity:x}"),
            Self::Content(content) => content_hash_hex(content),
        }
    }
}

/// A byte reference together with every name the Files facts give it.
///
/// A content reference may be shared by several differently named files, and
/// one file entity may itself be named more than once, so callers must not
/// pick one name as a winner. [`ResolvedFile::unique_name`] is `None` unless
/// exactly one name was observed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedFile {
    pub content: ContentHandle,
    pub names: Vec<String>,
}

impl ResolvedFile {
    pub fn unique_name(&self) -> Option<&str> {
        (self.names.len() == 1).then(|| self.names[0].as_str())
    }
}

/// Return the lowercase Blake3 digest addressed by a canonical file handle.
pub fn content_hash_hex(handle: ContentHandle) -> String {
    let hash: Inline<inlineencodings::Hash<inlineencodings::Blake3>> =
        inlineencodings::Handle::to_hash(handle);
    inlineencodings::Hash::<inlineencodings::Blake3>::to_hex(&hash).to_ascii_lowercase()
}

fn normalize_selector(input: &str) -> Result<String> {
    let trimmed = input.trim();
    let raw = trimmed.strip_prefix("files:").unwrap_or(trimmed);
    let selector = raw.to_ascii_lowercase();
    if selector.is_empty()
        || selector.len() > 64
        || !selector.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        anyhow::bail!(
            "invalid file selector '{selector}': expected 1 to 64 hexadecimal characters, optionally prefixed by 'files:'"
        );
    }
    Ok(selector)
}

/// The one thing a selector names, or the candidates it could mean.
///
/// This judges the caller's input, never the facts: a prefix shared by two
/// entities is a selector that needs more digits.
fn unambiguous<T>(
    selector: &str,
    matches: BTreeSet<T>,
    kind: &str,
    render: impl Fn(T) -> String,
) -> Result<T>
where
    T: Copy + Ord,
{
    match matches.len() {
        0 => anyhow::bail!("no {kind} matches selector '{selector}'"),
        1 => Ok(*matches.first().expect("one selector match")),
        _ => {
            let candidates = matches
                .into_iter()
                .map(render)
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::bail!("{kind} selector '{selector}' is ambiguous; candidates: {candidates}")
        }
    }
}

#[derive(Default)]
struct SelectorMatches {
    entities: BTreeSet<Id>,
    references: BTreeSet<FileReference>,
}

fn selector_matches<P: TriblePattern>(space: &P, selector: &str) -> SelectorMatches {
    let mut matches = SelectorMatches::default();
    for (entity, content) in find!(
        (entity: Id, content: ContentHandle),
        pattern!(space, [{
            ?entity @ metadata::tag: &KIND_FILE,
            file::content: ?content,
        }])
    ) {
        if format!("{entity:x}").starts_with(selector) {
            matches.entities.insert(entity);
            matches.references.insert(FileReference::Entity(entity));
        }
        if content_hash_hex(content).starts_with(selector) {
            matches.entities.insert(entity);
            matches.references.insert(FileReference::Content(content));
        }
    }
    for entity in find!(
        entity: Id,
        pattern!(space, [{ ?entity @ metadata::tag: &KIND_DIRECTORY }])
    )
    .chain(find!(
        entity: Id,
        pattern!(space, [{ ?entity @ metadata::tag: &KIND_IMPORT }])
    )) {
        if format!("{entity:x}").starts_with(selector) {
            matches.entities.insert(entity);
            matches.references.insert(FileReference::Entity(entity));
        }
    }
    matches
}

/// Resolve a file-faculty selector to exactly one entity.
///
/// Selectors are case-insensitive hexadecimal with an optional `files:`
/// prefix. A complete 32-character entity id is direct and intentionally does
/// not scan the facts. A complete 64-character digest resolves through
/// [`file::content`] and must identify exactly one canonical file entity. Any
/// other shorter selector is matched against eligible file, directory, and
/// import entity ids as well as file content digests. Matches are deduplicated
/// by entity, so one file matching through both its id and digest remains one
/// candidate.
pub fn resolve_selector<P: TriblePattern>(space: &P, input: &str) -> Result<Id> {
    let selector = normalize_selector(input)?;

    if selector.len() == 32 {
        return Id::from_hex(&selector).ok_or_else(|| anyhow!("invalid entity id '{selector}'"));
    }

    if selector.len() == 64 {
        let hash = inlineencodings::Hash::<inlineencodings::Blake3>::from_hex(&selector)
            .map_err(|_| anyhow!("invalid content hash '{selector}'"))?;
        let handle: ContentHandle = inlineencodings::Handle::from_hash(hash);
        let matches: BTreeSet<Id> = find!(
            entity: Id,
            pattern!(space, [{
                ?entity @ metadata::tag: &KIND_FILE,
                file::content: &handle,
            }])
        )
        .collect();
        return unambiguous(&selector, matches, "file entity", |entity| {
            format!("{entity:x}")
        });
    }

    unambiguous(
        &selector,
        selector_matches(space, &selector).entities,
        "file entity",
        |entity| format!("{entity:x}"),
    )
}

/// The content handle of a complete 64-character digest that no file entity
/// names, else `None`.
///
/// This is the open-world fallback: bytes may be resident without any Files
/// record describing them (Discord intake, for one, keeps an attachment as a
/// blob referenced from its own collection), and the digest alone still names
/// them exactly. Nothing is registered or copied; readers go to the blob.
pub fn unnamed_content<P: TriblePattern>(space: &P, input: &str) -> Result<Option<ContentHandle>> {
    let selector = normalize_selector(input)?;
    if selector.len() != 64 {
        return Ok(None);
    }
    let hash = inlineencodings::Hash::<inlineencodings::Blake3>::from_hex(&selector)
        .map_err(|_| anyhow!("invalid content hash '{selector}'"))?;
    let handle: ContentHandle = inlineencodings::Handle::from_hash(hash);
    let named = exists!(
        (entity: Id),
        pattern!(space, [{
            ?entity @ metadata::tag: &KIND_FILE,
            file::content: &handle,
        }])
    );
    Ok((!named).then_some(handle))
}

/// Resolve a selector to bytes and every name observed for them.
///
/// An entity reference reads that file's content and names; an entity read
/// with several contents names more than one byte string, so it is answered
/// with the candidates, each of which selects one. A content reference reads
/// every name of every file holding those bytes. A name whose text is not
/// resident or not UTF-8 is left out, like any value a typed read cannot
/// decode.
pub fn resolve_file<P: TriblePattern, R: BlobStoreGet>(
    space: &P,
    reader: &R,
    input: &str,
) -> Result<ResolvedFile> {
    let text = |handle: NameHandle| -> Option<String> {
        let name: anybytes::View<str> = reader.get(handle).ok()?;
        Some(name.to_string())
    };
    match resolve_reference(space, input)? {
        FileReference::Entity(id) => {
            let contents: BTreeSet<ContentHandle> = find!(
                content: ContentHandle,
                pattern!(space, [{ id @ metadata::tag: &KIND_FILE, file::content: ?content }])
            )
            .collect();
            let content = match contents.len() {
                0 => anyhow::bail!("files entity {id:x} is not a readable file"),
                1 => *contents.first().expect("one content"),
                count => anyhow::bail!(
                    "files entity {id:x} has {count} contents; select one: {}",
                    contents
                        .iter()
                        .map(|content| format!("files:{}", content_hash_hex(*content)))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            };
            let names = find!(
                name: NameHandle,
                pattern!(space, [{ id @ file::name: ?name }])
            )
            .filter_map(text)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
            Ok(ResolvedFile { content, names })
        }
        FileReference::Content(content) => {
            let names = find!(
                name: NameHandle,
                pattern!(space, [{
                    _?file @ metadata::tag: &KIND_FILE,
                    file::content: &content,
                    file::name: ?name,
                }])
            )
            .filter_map(text)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
            Ok(ResolvedFile { content, names })
        }
    }
}

/// Expand a selector to one canonical `files:` reference token.
///
/// This is deliberately distinct from [`resolve_selector`]. Entity selectors
/// collapse matches by entity and therefore reject one content hash shared by
/// multiple named files. Reference selectors collapse by reference token: the
/// shared content handle is one valid content reference, while entity ids are
/// separate reference tokens. Complete 32- and 64-character tokens are direct
/// and need no Files evidence; shorter prefixes are expanded from canonical
/// file, directory, and import records.
pub fn resolve_reference<P: TriblePattern>(space: &P, input: &str) -> Result<FileReference> {
    let selector = normalize_selector(input)?;

    if selector.len() == 32 {
        return Id::from_hex(&selector)
            .map(FileReference::Entity)
            .ok_or_else(|| anyhow!("invalid entity id '{selector}'"));
    }

    if selector.len() == 64 {
        let hash = inlineencodings::Hash::<inlineencodings::Blake3>::from_hex(&selector)
            .map_err(|_| anyhow!("invalid content hash '{selector}'"))?;
        return Ok(FileReference::Content(inlineencodings::Handle::from_hash(
            hash,
        )));
    }

    unambiguous(
        &selector,
        selector_matches(space, &selector).references,
        "file reference",
        |reference| format!("files:{}", reference.hex()),
    )
}

/// Best-effort media type inferred from a filename extension.
///
/// This table is shared because media type participates in canonical file
/// identity: two faculties must not classify the same named bytes differently
/// merely because they each grew their own extension mapping. Extensions are
/// ASCII-case-insensitive.
pub fn infer_media_type(path: &Path) -> &'static str {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match extension.as_str() {
        "pdf" => "application/pdf",
        "json" => "application/json",
        "toml" => "application/toml",
        "yaml" | "yml" => "application/yaml",
        "xml" => "application/xml",
        "csv" => "text/csv",
        "txt" => "text/plain",
        "md" | "markdown" => "text/markdown",
        "rs" => "text/x-rust",
        "py" => "text/x-python",
        "js" => "application/javascript",
        "ts" => "application/typescript",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        "tar" => "application/x-tar",
        "gz" | "gzip" => "application/gzip",
        "zip" => "application/zip",
        "wasm" => "application/wasm",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        _ => DEFAULT_MEDIA_TYPE,
    }
}

/// Reduce a source-provided name to the path-independent leaf used by file
/// identity. Both separator styles are recognized so records converge across
/// Unix, Windows, mail clients, and remote APIs.
pub fn leaf_name(name: &str) -> String {
    name.rsplit(['/', '\\'])
        .find(|part| !part.is_empty() && *part != "." && *part != "..")
        .unwrap_or("unnamed")
        .to_owned()
}

/// Parse a source-provided content type and return its canonical media-type
/// essence (parameters removed, type/subtype lower-cased).
///
/// An absent content type is represented by `application/octet-stream`.
/// Invalid non-empty values are rejected rather than becoming distinct bogus
/// media-type entities.
pub fn normalize_media_type(media_type: &str) -> Result<String> {
    let media_type = media_type.trim();
    if media_type.is_empty() {
        return Ok(DEFAULT_MEDIA_TYPE.to_owned());
    }
    let parsed = media_type
        .parse::<mime::Mime>()
        .map_err(|err| anyhow!("invalid file media type {media_type:?}: {err}"))?;
    Ok(parsed.essence_str().to_ascii_lowercase())
}

/// Normalize an untrusted protocol-provided content type, degrading malformed
/// values to the generic binary type. Source adapters use this when rejecting
/// one bad header would prevent an otherwise replayable sync cursor from
/// advancing; explicit user-facing MIME overrides should stay on the strict
/// [`normalize_media_type`] / [`stage`] path.
pub fn normalize_media_type_or_default(media_type: &str) -> String {
    normalize_media_type(media_type).unwrap_or_else(|_| DEFAULT_MEDIA_TYPE.to_owned())
}

/// Every media-type name handle of a file record, in handle order.
///
/// The join deliberately checks the target's kind as well as following the
/// relation: a dangling or wrongly-typed target is not a media type. A file
/// observed with several media types has each of them.
pub fn media_type_name_handles<P: TriblePattern>(space: &P, file_id: Id) -> BTreeSet<NameHandle> {
    find!(
        name: NameHandle,
        pattern!(space, [
            { file_id @ file::media_type: _?media_type },
            { _?media_type @ metadata::tag: &KIND_MEDIA_TYPE, metadata::name: ?name }
        ])
    )
    .collect()
}

/// Construct the canonical intrinsic entity for one normalized IANA media
/// type. Other faculties use this when describing bytes which are not Files
/// records while still sharing Files' media-type vocabulary.
pub fn media_type_fragment(media_type: &str) -> Result<Fragment> {
    let media_type = normalize_media_type(media_type)?;
    let mut fragment = Fragment::empty();
    let name = fragment.put::<UTF8String, _>(media_type);
    fragment += entity! {
        metadata::tag: &KIND_MEDIA_TYPE,
        metadata::name: name,
    };
    Ok(fragment)
}

/// Build one self-contained canonical file fragment.
///
/// `name` is a leaf name supplied by the caller, not a filesystem path. The
/// returned [`Fragment`] owns the content, name, and media-type-name blobs, so
/// callers can safely compose it into a larger fragment and publish that one
/// ownership unit through the native collection API.
pub fn stage<T>(bytes: T, name: impl Into<String>, media_type: &str) -> Result<Fragment>
where
    T: triblespace::core::blob::IntoBlob<RawBytes>,
{
    let media_type = normalize_media_type(media_type)?;
    let mut fragment = Fragment::empty();
    let content = fragment.put::<RawBytes, _>(bytes);
    let name = fragment.put::<UTF8String, _>(leaf_name(&name.into()));
    let media_type_name = fragment.put::<UTF8String, _>(media_type);
    let media_type = entity! {
        metadata::tag: &KIND_MEDIA_TYPE,
        metadata::name: media_type_name,
    };

    // Spreading the child fragment consumes its exported id into the relation
    // while folding its facts into the returned fragment. The file remains the
    // fragment's sole exported root.
    fragment += entity! {
        metadata::tag: &KIND_FILE,
        file::content: content,
        file::name: name,
        file::media_type*: media_type,
    };
    Ok(fragment)
}

/// Materialize the complete WRITE-authorized Files collection through an already
/// open pile.
///
/// Opening the scope introduces no second pile handle or lifecycle policy.
/// The returned immutable reader is the same coherent snapshot which admitted
/// the materialized facts.
pub fn materialize_collection(
    pile: &mut Pile,
    signer: &SigningKey,
) -> Result<(TribleSet, PileSnapshot)> {
    let collection = open_configured(
        pile,
        crate::schemas::files::DEFAULT_SCOPE_ID,
        signer.verifying_key(),
    )?;
    let store_snapshot = pile.snapshot().context("freeze Files store snapshot")?;
    let facts = store_snapshot
        .collection(collection)
        .context("attach Files collection")?
        .view::<TribleSet>()
        .context("read Files collection")?;
    Ok((facts, store_snapshot))
}

/// Publish one complete Files fragment through an already open pile.
///
/// This is the Files-first boundary used by attachment producers. The signed
/// record is appended before this returns; the caller retains ownership of the
/// pile's flush/close boundary.
pub fn commit_collection(
    pile: &mut Pile,
    signer: &SigningKey,
    fragment: Fragment,
) -> Result<CollectionCommit> {
    let collection = open_configured(
        pile,
        crate::schemas::files::DEFAULT_SCOPE_ID,
        signer.verifying_key(),
    )?;
    pile.commit(collection, signer, fragment)
        .map_err(|error| anyhow!("commit Files collection fragment: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use rand_core::OsRng;
    use triblespace::core::repo::memoryrepo::MemoryRepo;
    use triblespace::core::repo::BlobStoreGet;

    fn content_of(fragment: &Fragment) -> ContentHandle {
        find!(
            (content: ContentHandle),
            pattern!(fragment, [{ _?file @ file::content: ?content }])
        )
        .next()
        .expect("file content")
        .0
    }

    fn media_type_of(fragment: &Fragment) -> Id {
        find!(
            (media_type: Id),
            pattern!(fragment, [{ _?file @ file::media_type: ?media_type }])
        )
        .next()
        .expect("file media type")
        .0
    }

    fn facts_of(fragments: impl IntoIterator<Item = Fragment>) -> TribleSet {
        let mut facts = TribleSet::new();
        for fragment in fragments {
            facts += fragment;
        }
        facts
    }

    #[test]
    fn shared_content_keeps_all_names_and_has_no_name_winner() {
        let first = stage(b"shared".to_vec(), "alpha.txt", "text/plain").unwrap();
        let content = content_of(&first);
        let second = stage(b"shared".to_vec(), "beta.txt", "text/plain").unwrap();
        let mut fragment = first;
        fragment += second;
        let mut blobs = fragment.blobs().clone();
        let reader = blobs.snapshot().unwrap();

        let resolved = resolve_file(fragment.facts(), &reader, &content_hash_hex(content)).unwrap();
        assert_eq!(resolved.names, ["alpha.txt", "beta.txt"]);
        assert_eq!(resolved.unique_name(), None);
    }

    #[test]
    fn an_entity_named_twice_resolves_with_both_names_and_no_winner() {
        let mut fragment = stage(b"named".to_vec(), "record.txt", "text/plain").unwrap();
        let file_id = fragment.root().unwrap();
        let alternate = fragment.put::<UTF8String, _>("alternate.txt".to_owned());
        fragment += entity! { ExclusiveId::force_ref(&file_id) @ file::name: alternate };
        let mut blobs = fragment.blobs().clone();
        let reader = blobs.snapshot().unwrap();

        let resolved = resolve_file(fragment.facts(), &reader, &format!("{file_id:x}")).unwrap();
        assert_eq!(resolved.names, ["alternate.txt", "record.txt"]);
        assert_eq!(resolved.unique_name(), None);
        assert_eq!(resolved.content, content_of(&fragment));
    }

    #[test]
    fn an_entity_with_two_contents_resolves_to_its_candidates() {
        let mut fragment = stage(b"first".to_vec(), "record.txt", "text/plain").unwrap();
        let file_id = fragment.root().unwrap();
        let first = content_of(&fragment);
        let second = fragment.put::<RawBytes, _>(b"second".to_vec());
        fragment += entity! { ExclusiveId::force_ref(&file_id) @ file::content: second };
        let mut blobs = fragment.blobs().clone();
        let reader = blobs.snapshot().unwrap();

        let message = resolve_file(fragment.facts(), &reader, &format!("{file_id:x}"))
            .unwrap_err()
            .to_string();
        assert!(message.contains("2 contents"), "{message}");
        for content in [first, second] {
            assert!(message.contains(&format!("files:{}", content_hash_hex(content))));
            let resolved =
                resolve_file(fragment.facts(), &reader, &content_hash_hex(content)).unwrap();
            assert_eq!(resolved.names, ["record.txt"]);
        }
    }

    #[test]
    fn a_file_typed_twice_has_both_media_types() {
        let mut fragment = stage(b"typed".to_vec(), "record.bin", "text/plain").unwrap();
        let file_id = fragment.root().unwrap();
        let png = media_type_fragment("image/png").unwrap();
        fragment += entity! { ExclusiveId::force_ref(&file_id) @ file::media_type*: png };
        let mut blobs = fragment.blobs().clone();
        let reader = blobs.snapshot().unwrap();

        let names: BTreeSet<String> = media_type_name_handles(fragment.facts(), file_id)
            .into_iter()
            .map(|handle| {
                let name: anybytes::View<str> = reader.get(handle).unwrap();
                name.to_string()
            })
            .collect();
        assert_eq!(
            names,
            BTreeSet::from(["image/png".into(), "text/plain".into()])
        );
    }

    #[test]
    fn complete_entity_ids_are_direct_and_case_insensitive() {
        let expected = Id::from_hex("abcdef0123456789abcdef0123456789").unwrap();
        assert_eq!(
            resolve_selector(&TribleSet::new(), "files:ABCDEF0123456789ABCDEF0123456789").unwrap(),
            expected
        );
        assert_eq!(
            resolve_reference(&TribleSet::new(), "files:ABCDEF0123456789ABCDEF0123456789").unwrap(),
            FileReference::Entity(expected)
        );
    }

    #[test]
    fn entity_and_content_prefixes_resolve_without_a_minimum_length() {
        let file = stage(b"selector bytes".to_vec(), "selector.txt", "text/plain").unwrap();
        let file_id = file.root().unwrap();
        let content = content_of(&file);
        let hash = content_hash_hex(content);
        let facts = facts_of([file]);
        let id_hex = format!("{file_id:x}");
        assert_eq!(hash, hash.to_ascii_lowercase());

        assert_eq!(
            resolve_selector(&facts, &id_hex[..1].to_ascii_uppercase()).unwrap(),
            file_id
        );
        assert_eq!(
            resolve_selector(&facts, &id_hex[..12].to_ascii_uppercase()).unwrap(),
            file_id
        );
        assert_eq!(
            resolve_selector(
                &facts,
                &format!("files:{}", hash[..20].to_ascii_uppercase())
            )
            .unwrap(),
            file_id
        );
        assert_eq!(
            resolve_selector(&facts, &hash.to_ascii_uppercase()).unwrap(),
            file_id
        );

        let id_reference_prefix = (1..32)
            .map(|len| &id_hex[..len])
            .find(|prefix| {
                resolve_reference(&facts, prefix).ok() == Some(FileReference::Entity(file_id))
            })
            .expect("entity reference has an unambiguous prefix");
        assert_eq!(
            resolve_reference(&facts, &id_reference_prefix.to_ascii_uppercase()).unwrap(),
            FileReference::Entity(file_id)
        );
        assert_eq!(
            resolve_reference(&facts, &hash[..40].to_ascii_uppercase()).unwrap(),
            FileReference::Content(content)
        );
    }

    #[test]
    fn duplicate_content_is_ambiguous_by_entity_even_for_a_complete_hash() {
        let first = stage(b"shared bytes".to_vec(), "first.txt", "text/plain").unwrap();
        let second = stage(b"shared bytes".to_vec(), "second.txt", "text/plain").unwrap();
        let content = content_of(&first);
        let hash = content_hash_hex(content);
        let mut expected = vec![first.root().unwrap(), second.root().unwrap()];
        expected.sort();
        let facts = facts_of([first, second]);

        let message = resolve_selector(&facts, &hash).unwrap_err().to_string();
        assert!(message.contains("ambiguous"));
        for candidate in expected {
            assert!(message.contains(&format!("{candidate:x}")));
        }

        assert_eq!(
            resolve_reference(&facts, &hash).unwrap(),
            FileReference::Content(content)
        );
        assert_eq!(
            resolve_reference(&facts, &hash[..40]).unwrap(),
            FileReference::Content(content)
        );
        assert_eq!(
            resolve_reference(&TribleSet::new(), &hash).unwrap(),
            FileReference::Content(content)
        );
    }

    #[test]
    fn directory_and_import_ids_are_prefix_eligible() {
        let directory_id = *fucid();
        let import_id = *fucid();
        let directory = entity! { ExclusiveId::force_ref(&directory_id) @
            metadata::tag: &KIND_DIRECTORY,
        };
        let import = entity! { ExclusiveId::force_ref(&import_id) @
            metadata::tag: &KIND_IMPORT,
        };

        let directory_facts = facts_of([directory]);
        let directory_hex = format!("{directory_id:x}");
        assert_eq!(
            resolve_selector(&directory_facts, &directory_hex[..1]).unwrap(),
            directory_id
        );

        let import_facts = facts_of([import]);
        let import_hex = format!("{import_id:x}");
        assert_eq!(
            resolve_selector(&import_facts, &import_hex[..1]).unwrap(),
            import_id
        );
    }

    #[test]
    fn identical_file_records_have_identical_intrinsic_ids() {
        let left = stage(b"same bytes".to_vec(), "report.txt", "text/plain").unwrap();
        let right = stage(b"same bytes".to_vec(), "report.txt", "text/plain").unwrap();

        assert_eq!(left.root(), right.root());
        assert_eq!(left, right);
    }

    #[test]
    fn name_and_media_type_are_identity_but_bytes_still_deduplicate() {
        let text = stage(b"same bytes".to_vec(), "report.txt", "text/plain").unwrap();
        let renamed = stage(b"same bytes".to_vec(), "renamed.txt", "text/plain").unwrap();
        let retyped = stage(
            b"same bytes".to_vec(),
            "report.txt",
            "application/octet-stream",
        )
        .unwrap();

        assert_ne!(text.root(), renamed.root());
        assert_ne!(text.root(), retyped.root());
        assert_eq!(content_of(&text), content_of(&renamed));
        assert_eq!(content_of(&text), content_of(&retyped));
        assert_eq!(media_type_of(&text), media_type_of(&renamed));
        assert_ne!(media_type_of(&text), media_type_of(&retyped));
    }

    #[test]
    fn containing_paths_never_enter_file_identity() {
        let unix = stage(
            b"same bytes".to_vec(),
            "/tmp/archive/report.txt",
            "text/plain",
        )
        .unwrap();
        let windows = stage(
            b"same bytes".to_vec(),
            r"C:\archive\report.txt",
            "text/plain",
        )
        .unwrap();
        let bare = stage(b"same bytes".to_vec(), "report.txt", "text/plain").unwrap();

        assert_eq!(unix.root(), bare.root());
        assert_eq!(windows.root(), bare.root());
    }

    #[test]
    fn invalid_media_type_is_reported_instead_of_panicking() {
        let error = stage(
            Vec::<u8>::new(),
            "document.bin",
            "application/invalid\0mime",
        )
        .unwrap_err();
        assert!(error.to_string().contains("invalid file media type"));
    }

    #[test]
    fn media_type_normalization_is_lossless_and_deterministic() {
        assert_eq!(normalize_media_type("text/plain").unwrap(), "text/plain");
        assert_eq!(
            normalize_media_type(" TEXT/PLAIN; charset=UTF-8").unwrap(),
            "text/plain"
        );
        let vendor = "application/vnd.openxmlformats-officedocument.presentationml.presentation";
        assert_eq!(normalize_media_type(vendor).unwrap(), vendor);
        assert_eq!(
            normalize_media_type_or_default("not a media type"),
            DEFAULT_MEDIA_TYPE
        );
    }

    #[test]
    fn media_type_inference_is_case_insensitive_and_shared() {
        assert_eq!(infer_media_type(Path::new("README.MD")), "text/markdown");
        assert_eq!(
            infer_media_type(Path::new("deck.PPTX")),
            "application/vnd.openxmlformats-officedocument.presentationml.presentation"
        );
        assert_eq!(infer_media_type(Path::new("unknown")), DEFAULT_MEDIA_TYPE);
    }

    #[test]
    fn normalized_variants_converge_and_long_suffixes_remain_identity() {
        let plain = stage(b"same bytes".to_vec(), "document.bin", "text/plain").unwrap();
        let decorated = stage(
            b"same bytes".to_vec(),
            "document.bin",
            " TEXT/PLAIN; charset=UTF-8",
        )
        .unwrap();
        assert_eq!(plain.root(), decorated.root());
        assert_eq!(media_type_of(&plain), media_type_of(&decorated));

        let document = stage(
            b"same bytes".to_vec(),
            "document.bin",
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        )
        .unwrap();
        let sheet = stage(
            b"same bytes".to_vec(),
            "document.bin",
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        )
        .unwrap();
        assert_ne!(document.root(), sheet.root());
        assert_ne!(media_type_of(&document), media_type_of(&sheet));
    }

    #[test]
    fn staged_fragment_owns_content_name_and_joinable_media_type_name() {
        let file = stage(
            b"slides".to_vec(),
            "deck.pptx",
            "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        )
        .unwrap();
        let file_id = file.root().expect("file root");
        let content = content_of(&file);
        let file_name = find!(
            name: NameHandle,
            pattern!(&file, [{ file_id @ file::name: ?name }])
        )
        .next()
        .expect("file name");
        let media_type_id = find!(
            (media_type: Id),
            pattern!(&file, [{ file_id @ file::media_type: ?media_type }])
        )
        .next()
        .expect("media type relation")
        .0;

        assert!(exists!(
            (name: NameHandle),
            pattern!(&file, [{ media_type_id @ metadata::tag: &KIND_MEDIA_TYPE, metadata::name: ?name }])
        ));
        let name_handle = *media_type_name_handles(file.facts(), file_id)
            .first()
            .expect("media type name");
        assert_eq!(
            Some(name_handle),
            find!(
                (name: NameHandle),
                pattern!(&file, [{ media_type_id @ metadata::name: ?name }])
            )
            .next()
            .map(|(name,)| name)
        );
        let mut blobs = file.blobs().clone();
        let reader = blobs.snapshot().expect("fragment blob reader");
        let name: anybytes::View<str> = reader
            .get::<anybytes::View<str>, UTF8String>(name_handle)
            .expect("staged media type name");
        assert_eq!(
            name.as_ref(),
            "application/vnd.openxmlformats-officedocument.presentationml.presentation"
        );
        let content: anybytes::Bytes = reader.get(content).expect("staged content");
        assert_eq!(content.as_ref(), b"slides");
        let file_name: anybytes::View<str> = reader.get(file_name).expect("staged name");
        assert_eq!(file_name.as_ref(), "deck.pptx");
    }

    #[test]
    fn complete_fragment_survives_native_collection_commit_and_materialization() {
        let signer = SigningKey::generate(&mut OsRng);
        let mut store = MemoryRepo::default();
        let collection = crate::collection_names::open(
            &mut store,
            crate::schemas::files::DEFAULT_SCOPE_ID,
            signer.verifying_key(),
        )
        .unwrap();
        let file = stage(
            b"slides".to_vec(),
            "deck.pptx",
            "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        )
        .unwrap();
        let file_id = file.root().expect("file root");

        // The Fragment is the ownership unit: collapsing it into a TribleSet
        // would discard its attachment store before commit can persist the
        // closure.
        store
            .commit(collection, &signer, file)
            .expect("commit canonical file");
        let reader = store.snapshot().expect("freeze files store snapshot");
        let (catalog, _) =
            crate::storage::read_fact_collection(collection, &reader).expect("materialize files");
        let content_handle = find!(
            content: ContentHandle,
            pattern!(&catalog, [{ file_id @ file::content: ?content }])
        )
        .next()
        .expect("persisted content handle");
        let file_name_handle = find!(
            name: NameHandle,
            pattern!(&catalog, [{ file_id @ file::name: ?name }])
        )
        .next()
        .expect("persisted file name handle");
        let name_handle = *media_type_name_handles(&catalog, file_id)
            .first()
            .expect("canonical media type");
        let name: anybytes::View<str> = reader
            .get::<anybytes::View<str>, UTF8String>(name_handle)
            .expect("persisted media type name");
        assert_eq!(
            name.as_ref(),
            "application/vnd.openxmlformats-officedocument.presentationml.presentation"
        );
        let content: anybytes::Bytes = reader.get(content_handle).expect("persisted content");
        assert_eq!(content.as_ref(), b"slides");
        let file_name: anybytes::View<str> =
            reader.get(file_name_handle).expect("persisted file name");
        assert_eq!(file_name.as_ref(), "deck.pptx");
    }
}
