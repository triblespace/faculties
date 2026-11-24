//! The files a ChatGPT data export's library holds and no conversation's nodes do.
//!
//! An export lists its file library in `library_files.json` and ships many of
//! the files beside it. A file a conversation of the export names as media or
//! as an attachment is part of that conversation's nodes
//! ([`crate::archive_chatgpt`]); a mention anywhere else, such as a citation's
//! URL, makes no node. The other files have no node to belong to, so each
//! becomes an observation and no node ([`schema::library_file`]): this library
//! holds these bytes, recorded then, as that generation, out of that
//! conversation. Nothing is said about order, and nothing about who made a
//! file: no member states a direction.
//!
//! The library's record names its actor (`user`) and action (`create`). They
//! are the same for uploads and for generated images, so they tell nothing
//! apart, and any other value fails the unit: it would be a claim nobody has
//! read yet. So would an upload origin on a file that is observed here.
//!
//! What the record states of its file's size and SHA-256 must hold of the
//! bytes, or the unit fails. Every record is accounted for: observed, named by
//! a conversation, or not shipped ([`ProjectionSummary`]).

use std::collections::BTreeMap;
use std::path::Path;

use anybytes::{Bytes, View};
use anyhow::{anyhow, bail, Context, Result};
use hifitime::Epoch;
use sha2::{Digest, Sha256};
use triblespace::core::inline::TryToInline;
use triblespace::prelude::inlineencodings::NsTAIInterval;
use triblespace::prelude::*;

use crate::archive_chatgpt::{self, ExportFiles};
use crate::archive_shape::{self, Rule, Shape};
use crate::archive_source::{self, JsonNumber, ProjectedSource};
use crate::blockdag::{self, LibraryFileAnnotations};
use crate::files;
use crate::schemas::blockdag as schema;

/// The library's own file in an export.
const LIBRARY: &str = "library_files.json";

/// Accounting returned after every observation reached `emit`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProjectionSummary {
    /// Records the library lists: the three counts below added up.
    pub records_seen: usize,
    /// Files observed.
    pub observed: usize,
    /// Records of files a conversation names, left to its nodes.
    pub named_by_conversation: usize,
    /// Records of files the export names and does not ship.
    pub not_shipped: usize,
    /// Bytes of the observed files.
    pub bytes: u64,
    /// Observed files by media type.
    pub media_types: BTreeMap<String, usize>,
    /// Observed files whose record names a generation.
    pub generations: usize,
    /// Observed files whose record names a conversation.
    pub located: usize,
    /// Members left out that held a value, by path, with how many did.
    pub dropped: BTreeMap<&'static str, usize>,
}

use Rule::{Drop, Is, Null, Opt, Read};

const LIST: &str = "state of the library list, not of the file";
const STORE: &str = "where and how the provider stores the file";
const CLOCK: &str = "a moment of the provider's processing, beside the record's creation";
const DERIVED: &str = "derived from members that are kept";

const RECORD: Shape = Shape {
    name: "library record",
    members: &[
        ("app_id", Opt(&Drop("app_id", STORE))),
        ("content_backing_kind", Drop("content_backing_kind", STORE)),
        (
            "context_scopes_v2",
            Opt(&Drop("context_scopes_v2", "access scopes")),
        ),
        ("created_at", Opt(&Drop("created_at", CLOCK))),
        (
            "current_version_number",
            Opt(&Drop("current_version_number", STORE)),
        ),
        ("deleted_at", Opt(&Null)),
        ("deletion_origin", Opt(&Null)),
        ("deletion_reason", Opt(&Null)),
        ("directory_id", Opt(&Drop("directory_id", LIST))),
        ("etag", Opt(&Drop("etag", STORE))),
        ("file_extension", Drop("file_extension", DERIVED)),
        ("file_id", Read),
        ("file_name", Read),
        (
            "file_name_provenance",
            Opt(&Drop(
                "file_name_provenance",
                "how the list came by the name; uploads and generations share a value",
            )),
        ),
        ("file_name_revision", Opt(&Drop("file_name_revision", LIST))),
        (
            "file_processed_time",
            Opt(&Drop("file_processed_time", CLOCK)),
        ),
        ("file_size_bytes", Opt(&Read)),
        ("file_upload_time", Drop("file_upload_time", CLOCK)),
        ("id", Drop("id", "the list's own key")),
        ("image_gen_generation_id", Opt(&Read)),
        ("is_delayed", Opt(&Drop("is_delayed", STORE))),
        ("is_visible", Opt(&Drop("is_visible", LIST))),
        (
            "knowledge_store_id",
            Opt(&Drop("knowledge_store_id", STORE)),
        ),
        ("last_used_at", Opt(&Drop("last_used_at", LIST))),
        (
            "library_artifact_type",
            Opt(&Drop("library_artifact_type", LIST)),
        ),
        (
            "library_file_category",
            Drop("library_file_category", DERIVED),
        ),
        (
            "metadata_updated_at",
            Opt(&Drop("metadata_updated_at", LIST)),
        ),
        ("mime_type", Read),
        ("modified_at", Opt(&Drop("modified_at", LIST))),
        ("normalized_name", Opt(&Drop("normalized_name", DERIVED))),
        ("object_version", Opt(&Drop("object_version", STORE))),
        ("object_version_2", Opt(&Drop("object_version_2", STORE))),
        ("origination_message_id", Opt(&Read)),
        ("origination_thread_id", Opt(&Read)),
        ("producer_metadata", Opt(&Read)),
        ("record_creation_time", Read),
        (
            "request_read_version",
            Opt(&Drop("request_read_version", STORE)),
        ),
        ("root_directory_id", Opt(&Drop("root_directory_id", LIST))),
        ("sha256_digest", Opt(&Read)),
        ("state", Drop("state", LIST)),
        (
            "thumbnail_sources",
            Opt(&Drop("thumbnail_sources", "small copies the list shows")),
        ),
        ("trashed_at", Opt(&Null)),
        ("ttl", Opt(&Drop("ttl", STORE))),
        ("updated_at", Opt(&Drop("updated_at", LIST))),
        (
            "uploading_account_user_id",
            Drop("uploading_account_user_id", "the account, one for all"),
        ),
        ("version_created_at", Drop("version_created_at", CLOCK)),
        ("version_provenance", Read),
    ],
};

const VERSION: Shape = Shape {
    name: "version provenance",
    members: &[
        ("action", Is("create")),
        ("actor", Is("user")),
        (
            "actor_id",
            Drop("version_provenance.actor_id", "the account, one for all"),
        ),
        ("reason", Null),
    ],
};

const PRODUCER: Shape = Shape {
    name: "producer metadata",
    members: &[("upload_origin", Read)],
};

/// One record's kept members.
struct Record {
    file_id: View<str>,
    size: Option<u64>,
    sha256: Option<View<str>>,
    upload_origin: Option<View<str>>,
    annotations: LibraryFileAnnotations,
}

fn read_record(value: Bytes, summary: &mut ProjectionSummary) -> Result<Record> {
    let dropped = &mut summary.dropped;
    let mut record = archive_shape::open(&RECORD, value, dropped)?;
    archive_shape::open(&VERSION, record.take("version_provenance")?, dropped)?.finish()?;
    let upload_origin = match record.optional("producer_metadata") {
        Some(producer) => {
            let mut producer = archive_shape::open(&PRODUCER, producer, dropped)?;
            Some(producer.text("upload_origin")?)
        }
        None => None,
    };
    let size = record
        .optional("file_size_bytes")
        .map(|value| match archive_source::json_number(&value) {
            Some(JsonNumber::PosInt(size)) => Ok(size),
            _ => Err(anyhow!("library record.file_size_bytes is not a size")),
        })
        .transpose()?;
    let sha256 = record.optional_text("sha256_digest")?;
    let file_id = record.text("file_id")?;
    let media_type = record.text("mime_type")?;
    let media_type = media_type.as_ref().trim();
    let annotations = LibraryFileAnnotations {
        name: Some(record.text("file_name")?),
        media_type: (!media_type.is_empty())
            .then(|| files::normalize_media_type(media_type))
            .transpose()?,
        asset_pointer: Some(archive_chatgpt::canonical_pointer(file_id.as_ref())),
        recorded_at: Some(instant(record.text("record_creation_time")?.as_ref())?),
        source_locator: record
            .optional_text("origination_thread_id")?
            .map(|id| format!("conversation/{}", id.as_ref())),
        generation: record.optional_text("image_gen_generation_id")?,
        origin_message: record.optional_text("origination_message_id")?,
    };
    record.finish()?;
    Ok(Record {
        file_id,
        size,
        sha256,
        upload_origin,
        annotations,
    })
}

/// An instant the export spells as RFC 3339 with its offset.
fn instant(text: &str) -> Result<Inline<NsTAIInterval>> {
    let epoch = Epoch::from_gregorian_str(text)
        .map_err(|_| anyhow!("library record.record_creation_time is not a time"))?;
    (epoch, epoch)
        .try_to_inline()
        .map_err(|_| anyhow!("library record.record_creation_time is not representable"))
}

/// Project the library of the export directory `root`, or of the directory
/// its `library_files.json` lies in.
///
/// Nothing is emitted unless every record was read and every observed file's
/// bytes are what its record says.
pub fn project_path<F>(path: &Path, mut emit: F) -> Result<ProjectionSummary>
where
    F: FnMut(ProjectedSource) -> Result<()>,
{
    let root = if path.is_dir() {
        path
    } else if path.file_name().is_some_and(|name| name == LIBRARY) {
        match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => Path::new("."),
        }
    } else {
        bail!("{} is not a ChatGPT {LIBRARY}", path.display());
    };
    let source_path = root.join(LIBRARY);
    let mapped = archive_source::map_immutable_file(&source_path)?;
    let document = archive_source::json_document(mapped).map_err(|error| anyhow!(error))?;
    let records = archive_source::json_elements(&document)
        .ok_or_else(|| anyhow!("{LIBRARY} is not a JSON array"))?;
    let named = archive_chatgpt::named_file_ids(root)?;
    let export = ExportFiles::index(root)?;

    let mut summary = ProjectionSummary::default();
    let mut projected = Vec::new();
    for (index, value) in records.into_iter().enumerate() {
        let observation = observe(value, &named, &export, &mut summary)
            .with_context(|| format!("library record {index}"))?;
        projected.extend(observation);
    }
    for (fragment, identity) in projected {
        emit(ProjectedSource {
            source_path: source_path.clone(),
            fragment,
            identity,
            record: None,
        })?;
    }
    Ok(summary)
}

/// One record's observation, if its file is shipped and no conversation
/// names it.
fn observe(
    value: Bytes,
    named: &std::collections::HashSet<String>,
    export: &ExportFiles,
    summary: &mut ProjectionSummary,
) -> Result<Option<(Fragment, blockdag::Identity)>> {
    summary.records_seen += 1;
    let record = read_record(value, summary)?;
    let Some(file) = export.unique(record.file_id.as_ref())? else {
        summary.not_shipped += 1;
        return Ok(None);
    };
    if named.contains(record.file_id.as_ref()) {
        summary.named_by_conversation += 1;
        return Ok(None);
    }
    if record.upload_origin.is_some() {
        bail!("a file no conversation names states an upload origin");
    }
    let bytes = archive_source::map_immutable_file(file)?;
    let len = bytes.len() as u64;
    if record.size.is_some_and(|size| size != len) {
        bail!("the file is not the size its record states");
    }
    if let Some(stated) = record.sha256 {
        if hex::encode(Sha256::digest(bytes.as_ref())) != stated.as_ref().to_ascii_lowercase() {
            bail!("the file's bytes do not have the SHA-256 its record states");
        }
    }
    let annotations = record.annotations;
    summary.observed += 1;
    summary.bytes += len;
    summary.generations += usize::from(annotations.generation.is_some());
    summary.located += usize::from(annotations.source_locator.is_some());
    let media_type = annotations.media_type.as_deref().unwrap_or("untyped");
    *summary
        .media_types
        .entry(media_type.to_owned())
        .or_default() += 1;
    blockdag::library_file(
        schema::source_projection::SOURCE_CHATGPT,
        bytes,
        annotations,
    )
    .map(Some)
}

#[cfg(test)]
#[path = "archive_chatgpt_library/tests.rs"]
mod tests;
