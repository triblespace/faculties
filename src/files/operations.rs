//! Files operations over maintained, immutable collection views.
//!
//! Callers use typed operations directly; no CLI invocation or MCP value enters here.

use crate::clock;
use crate::collection_names::{configured_handle, open, open_configured_acquiring};
#[cfg(test)]
use crate::collection_names::open_exact_in;
use crate::files as file_capability;
use crate::out::Out;
use crate::schemas::files::{file, DEFAULT_SCOPE_ID, KIND_DIRECTORY, KIND_FILE, KIND_IMPORT};
#[cfg(test)]
use crate::storage::FactRead;
use crate::storage::{read, AcquiringReader, FactArchive, FacultySnapshot, FacultyStore, Storage};
use anyhow::{bail, Context, Result};
use ed25519_dalek::SigningKey;
use hifitime::efmt::consts::ISO8601_DATE;
use hifitime::efmt::Formatter;
use hifitime::Epoch;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use triblespace::core::blob::encodings::simplearchive::SimpleArchive;
use triblespace::core::blob::encodings::succinctarchive::{
    Rank9AcceleratedSuccinctArchiveBlob, SuccinctArchiveBlob,
};
use triblespace::core::collection::{Collection, CollectionStoreExt};
use triblespace::core::metadata;
use triblespace::core::query::TriblePattern;
use triblespace::core::repo::async_store::AsyncBlobStoreAcquire;
#[cfg(test)]
use triblespace::core::repo::pile::Pile;
use triblespace::core::repo::pile::PileSnapshot;
use triblespace::core::repo::{BlobStoreGet, BlobStoreList, SnapshotSource};
use triblespace::prelude::*;
#[cfg(feature = "wemm")]
use triblespace_search::nvfp4::ReconstructedCosines;

// ── type aliases ─────────────────────────────────────────────────────────
type FileHandle = Inline<inlineencodings::Handle<blobencodings::RawBytes>>;
type TextHandle = Inline<inlineencodings::Handle<blobencodings::UTF8String>>;

// ── helpers ──────────────────────────────────────────────────────────────

fn now_tai() -> Result<Inline<inlineencodings::NsTAIInterval>> {
    clock::point_now()
}

fn interval_key(interval: Inline<inlineencodings::NsTAIInterval>) -> i128 {
    let (lower, _): (Epoch, Epoch) = interval.try_from_inline().expect("valid TAI interval");
    lower.to_tai_duration().total_nanoseconds()
}

fn format_date(tai_ns: i128) -> String {
    const NANOS_PER_CENTURY: i128 = 3_155_760_000_000_000_000;
    let centuries = (tai_ns / NANOS_PER_CENTURY) as i16;
    let nanos = (tai_ns % NANOS_PER_CENTURY) as u64;
    let dur = hifitime::Duration::from_parts(centuries, nanos);
    let epoch = Epoch::from_tai_duration(dur);
    Formatter::new(epoch, ISO8601_DATE).to_string()
}

fn fmt_id(id: Id) -> String {
    format!("{id:x}")
}

fn handle_hex(h: FileHandle) -> String {
    file_capability::content_hash_hex(h)
}

fn human_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    if bytes >= GB {
        format!("{:.1} GiB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MiB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KiB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

// ── query helpers ────────────────────────────────────────────────────────

fn read_name<P: TriblePattern, R: BlobStoreGet>(
    space: &P,
    reader: &R,
    eid: Id,
) -> Result<Option<String>> {
    let Some((h,)) = find!(
        (h: TextHandle),
        pattern!(space, [{ eid @ file::name: ?h }])
    )
    .next() else {
        return Ok(None);
    };
    let blob: Blob<blobencodings::UTF8String> = reader.get(h).context("read file name")?;
    Ok(View::<str>::try_from_blob(blob)
        .ok()
        .map(|view| view.as_ref().to_string()))
}

fn read_mime<P: TriblePattern, R: BlobStoreGet>(
    space: &P,
    reader: &R,
    eid: Id,
) -> Result<Option<String>> {
    let Some(handle) = file_capability::media_type_name_handle(space, eid) else {
        return Ok(None);
    };
    let blob: Blob<blobencodings::UTF8String> =
        reader.get(handle).context("read file media type")?;
    Ok(View::<str>::try_from_blob(blob)
        .ok()
        .map(|view| view.as_ref().to_string()))
}

fn content_handle_of<P: TriblePattern>(space: &P, eid: Id) -> Option<FileHandle> {
    find!(
        (h: FileHandle),
        pattern!(space, [{ eid @ file::content: ?h }])
    )
    .next()
    .map(|(h,)| h)
}

fn is_file<P: TriblePattern>(space: &P, id: Id) -> bool {
    exists!(
        (h: FileHandle),
        pattern!(space, [{ id @ metadata::tag: &KIND_FILE, file::content: ?h }])
    )
}

fn is_directory<P: TriblePattern>(space: &P, id: Id) -> bool {
    exists!(
        (c: Id),
        pattern!(space, [{ id @ metadata::tag: &KIND_DIRECTORY, file::children: ?c }])
    )
}

fn is_import<P: TriblePattern>(space: &P, id: Id) -> bool {
    exists!(
        (r: Id),
        pattern!(space, [{ id @ metadata::tag: &KIND_IMPORT, file::root: ?r }])
    )
}

fn children_of<P: TriblePattern>(space: &P, id: Id) -> Vec<Id> {
    find!(
        (c: Id),
        pattern!(space, [{ id @ file::children: ?c }])
    )
    .map(|(c,)| c)
    .collect()
}

fn root_of<P: TriblePattern>(space: &P, id: Id) -> Option<Id> {
    find!(
        (r: Id),
        pattern!(space, [{ id @ file::root: ?r }])
    )
    .next()
    .map(|(r,)| r)
}

fn imported_at_of<P: TriblePattern>(space: &P, eid: Id) -> Option<i128> {
    find!(
        (ts: Inline<inlineencodings::NsTAIInterval>),
        pattern!(space, [{ eid @ file::imported_at: ?ts }])
    )
    .next()
    .map(|(ts,)| interval_key(ts))
}

fn source_path_of<P: TriblePattern, R: BlobStoreGet>(
    space: &P,
    reader: &R,
    eid: Id,
) -> Result<Option<String>> {
    let Some((h,)) = find!(
        (h: TextHandle),
        pattern!(space, [{ eid @ file::source_path: ?h }])
    )
    .next() else {
        return Ok(None);
    };
    let blob: Blob<blobencodings::UTF8String> = reader.get(h).context("read import source path")?;
    Ok(View::<str>::try_from_blob(blob)
        .ok()
        .map(|view| view.as_ref().to_string()))
}

fn tags_of<P: TriblePattern>(space: &P, eid: Id) -> Vec<String> {
    find!(
        t: String,
        pattern!(space, [{ eid @ file::tag: ?t }])
    )
    .collect()
}

// ── native collection boundary ───────────────────────────────────────────

/// Register the Files collection for append-only work through its storage
/// handle. Commands that construct a complete fragment locally do not
/// pay to reconstruct the existing collection value.
fn with_files_store<T>(
    storage: &Storage,
    f: impl FnOnce(
        &mut FacultyStore,
        Collection<SimpleArchive>,
        &SigningKey,
        &std::sync::Arc<tokio::runtime::Runtime>,
    ) -> Result<T>,
) -> Result<T> {
    // Authority is durable and explicit: ordinary Files commands never mint a
    // new signer and never fall back to an ephemeral identity.
    storage.with_store(|store, signer, runtime| {
        let collection = if configured_handle(DEFAULT_SCOPE_ID)?.is_some() {
            open_configured_acquiring(store, DEFAULT_SCOPE_ID, signer.verifying_key(), runtime)?
        } else {
            open(store, DEFAULT_SCOPE_ID, signer.verifying_key())
                .context("register signer-private Files descriptor")?
        };
        f(store, collection, signer, runtime)
    })
}

fn ensure_files_after_commit(
    store: &mut FacultyStore,
    collection: Collection<SimpleArchive>,
    signer: &SigningKey,
    runtime: &tokio::runtime::Runtime,
) -> Result<()> {
    drop(
        runtime
            .block_on(crate::storage::ensure_downstream(store, collection, signer))
            .context("Files facts were committed, but ensuring their derived views failed")?,
    );
    Ok(())
}

/// Attach one immutable shard-preserving Files view for commands whose result
/// or mutation depends on facts already present in the collection.
///
/// The views are attached as they stand: what a write ensured after its own
/// commit, what the maintenance worker carried, or what replicated from a
/// node that did. A read never maintains, whatever it may write: on
/// 2026-09-14 a read on stars failed for want of WRITE on the Files Succinct
/// target, and the answer is not wider rights.
fn with_files_view<T>(
    storage: &Storage,
    f: impl FnOnce(
        &mut FacultyStore,
        Collection<SimpleArchive>,
        &SigningKey,
        &FactArchive,
        &FacultySnapshot,
        &std::sync::Arc<tokio::runtime::Runtime>,
    ) -> Result<T>,
) -> Result<T> {
    with_files_store(storage, |store, collection, signer, runtime| {
        files_view_in(store, collection, signer, runtime, f)
    })
}

/// Attach the Files views of one already-opened source collection; see
/// [`with_files_view`] for the maintenance rule.
fn files_view_in<T>(
    store: &mut FacultyStore,
    collection: Collection<SimpleArchive>,
    signer: &SigningKey,
    runtime: &std::sync::Arc<tokio::runtime::Runtime>,
    f: impl FnOnce(
        &mut FacultyStore,
        Collection<SimpleArchive>,
        &SigningKey,
        &FactArchive,
        &FacultySnapshot,
        &std::sync::Arc<tokio::runtime::Runtime>,
    ) -> Result<T>,
) -> Result<T> {
    {
        let succinct = store
            .attach::<SuccinctArchiveBlob>(collection, ())
            .context("register Files Succinct collection")?;
        let rank9 = store
            .attach::<Rank9AcceleratedSuccinctArchiveBlob>(collection, succinct)
            .context("register Files Rank9 collection")?;
        let reader = store
            .snapshot()
            .context("freeze the Files views as they stand")?;
        let acquiring = AcquiringReader::new(reader.clone(), runtime.clone());
        let space = crate::storage::acquire_facts(&acquiring, rank9)
            .context("read Files fact collection")?;
        f(store, collection, signer, &space, &reader, runtime)
    }
}

// ── tree builder ─────────────────────────────────────────────────────────

struct TreeStats {
    files: usize,
    dirs: usize,
    bytes: u64,
}

/// Build a Merkle tree from a filesystem path, bottom-up.
/// Returns a Fragment whose root is the top-level entity and whose
/// facts contain the entire tree.
fn print_fs_tree(
    path: &Path,
    prefix: &str,
    child_prefix: &str,
    stats: &mut TreeStats,
    out: &mut Out<'_>,
) -> Result<()> {
    let meta = fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or(".");

    if meta.is_file() {
        let size = meta.len();
        stats.bytes += size;
        stats.files += 1;
        let mime = file_capability::infer_media_type(path);
        out.line(format!("{prefix}{name}  ({mime}, {})", human_size(size)))?;
    } else if meta.is_dir() {
        stats.dirs += 1;
        let mut dirs: Vec<(String, PathBuf)> = Vec::new();
        let mut files: Vec<(String, PathBuf)> = Vec::new();
        for entry in fs::read_dir(path).with_context(|| format!("read dir {}", path.display()))? {
            let entry = entry?;
            let ename = entry.file_name().to_string_lossy().to_string();
            if ename.starts_with('.') {
                continue;
            }
            if entry.file_type()?.is_dir() {
                dirs.push((ename, entry.path()));
            } else {
                files.push((ename, entry.path()));
            }
        }
        dirs.sort_by(|a, b| a.0.cmp(&b.0));
        files.sort_by(|a, b| a.0.cmp(&b.0));

        out.line(format!("{prefix}{name}/"))?;
        let all: Vec<_> = dirs.into_iter().chain(files).collect();
        for (i, (_, child_path)) in all.iter().enumerate() {
            let last = i == all.len() - 1;
            let connector = if last { "└── " } else { "├── " };
            let continuation = if last { "    " } else { "│   " };
            print_fs_tree(
                child_path,
                &format!("{child_prefix}{connector}"),
                &format!("{child_prefix}{continuation}"),
                stats,
                out,
            )?;
        }
    }
    Ok(())
}

fn build_tree(path: &Path, mime_override: Option<&str>, stats: &mut TreeStats) -> Result<Fragment> {
    let meta = fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;

    if meta.is_file() {
        let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
        stats.bytes += bytes.len() as u64;
        let mime = mime_override.unwrap_or_else(|| file_capability::infer_media_type(path));
        let name_str = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unnamed");

        stats.files += 1;
        let frag = file_capability::stage(bytes, name_str, mime)?;
        Ok(frag)
    } else if meta.is_dir() {
        // Collect children sorted by name for deterministic ordering.
        let mut entries: BTreeMap<String, PathBuf> = BTreeMap::new();
        for entry in fs::read_dir(path).with_context(|| format!("read dir {}", path.display()))? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            // Skip hidden files and common noise.
            if name.starts_with('.') {
                continue;
            }
            entries.insert(name, entry.path());
        }

        let mut children = Fragment::default();

        for (_name, child_path) in &entries {
            let child_frag = build_tree(child_path, None, stats)?;
            children += child_frag;
        }

        let dir_name = path.file_name().and_then(|n| n.to_str()).unwrap_or(".");
        let mut directory = Fragment::empty();
        let name_h: TextHandle = directory.put(dir_name.to_string());
        stats.dirs += 1;
        directory += entity! {
            metadata::tag: &KIND_DIRECTORY,
            file::name: name_h,
            file::children*: children
        };
        Ok(directory)
    } else {
        bail!("unsupported file type: {}", path.display());
    }
}

// ── commands ─────────────────────────────────────────────────────────────

fn cmd_add_dry_run(path: &Path, tags: &[String], out: &mut Out<'_>) -> Result<()> {
    let abs_path =
        fs::canonicalize(path).with_context(|| format!("canonicalize {}", path.display()))?;
    let mut stats = TreeStats {
        files: 0,
        dirs: 0,
        bytes: 0,
    };
    print_fs_tree(&abs_path, "", "", &mut stats, out)?;
    out.line("")?;
    out.line(format!(
        "Would import: {} files, {} dirs, {}",
        stats.files,
        stats.dirs,
        human_size(stats.bytes),
    ))?;
    if !tags.is_empty() {
        out.line(format!("Tags: {}", tags.join(", ")))?;
    }
    Ok(())
}

fn cmd_add(
    pile: &mut FacultyStore,
    collection: Collection<SimpleArchive>,
    signer: &SigningKey,
    runtime: &std::sync::Arc<tokio::runtime::Runtime>,
    path: &Path,
    mime_override: Option<&str>,
    tags: &[String],
    out: &mut Out<'_>,
) -> Result<()> {
    let abs_path =
        fs::canonicalize(path).with_context(|| format!("canonicalize {}", path.display()))?;

    let source = abs_path.to_string_lossy().to_string();

    let mut stats = TreeStats {
        files: 0,
        dirs: 0,
        bytes: 0,
    };
    let tree = build_tree(&abs_path, mime_override, &mut stats)?;
    let root_id = tree.root().expect("tree has a root");
    let root_content = content_handle_of(tree.facts(), root_id);

    // Create import entity, spreading the tree into it.
    let ts = now_tai()?;
    let mut import_frag = Fragment::empty();
    let source_h: TextHandle = import_frag.put(source.clone());
    import_frag += entity! {
        metadata::tag: &KIND_IMPORT,
        file::root: &root_id,
        file::imported_at: ts,
        file::source_path: source_h
    };
    let import_id = import_frag.root().expect("import has an id");
    let mut change = tree;
    change += import_frag;

    // Tags go on the import entity.
    for t in tags {
        change += entity! { ExclusiveId::force_ref(&import_id) @ file::tag: t.as_str() };
    }

    pile.commit(collection, signer, change)
        .context("commit Files import")?;
    // Saving never embeds: on 2026-10-06 one save on a gb10 spent 76 minutes
    // embedding the whole backlog. Rows come from `files index` only.
    ensure_files_after_commit(pile, collection, signer, runtime)?;

    if stats.dirs > 0 {
        out.line(format!(
            "Imported {} ({} files, {} dirs, {})",
            abs_path.display(),
            stats.files,
            stats.dirs,
            human_size(stats.bytes),
        ))?;
    } else {
        // Single file — show the content hash.
        let h = root_content.ok_or_else(|| anyhow::anyhow!("missing content handle"))?;
        let hash = handle_hex(h);
        let name = abs_path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
        let mime = file_capability::normalize_media_type(
            mime_override.unwrap_or_else(|| file_capability::infer_media_type(&abs_path)),
        )?;
        out.line(format!("{}  {}  ({})", hash, name, human_size(stats.bytes)))?;
        if mime.starts_with("image/") {
            out.line(format!("![{name}](files:{hash})"))?;
        }
    }
    out.line(format!("Import: {}", fmt_id(import_id)))?;
    Ok(())
}

fn cmd_fetch(
    pile: &mut FacultyStore,
    collection: Collection<SimpleArchive>,
    signer: &SigningKey,
    runtime: &tokio::runtime::Runtime,
    url: &str,
    mime_override: Option<&str>,
    name_override: Option<&str>,
    tags: &[String],
    max_bytes: usize,
    out: &mut Out<'_>,
) -> Result<()> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("playground-files-faculty/0")
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .context("build http client")?;
    let response = client
        .get(url)
        .send()
        .with_context(|| format!("fetch {url}"))?
        .error_for_status()
        .with_context(|| format!("fetch {url}"))?;

    let header_mime = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string);
    use std::io::Read;
    let read_limit = u64::try_from(max_bytes)?
        .checked_add(1)
        .context("max_bytes is too large")?;
    let mut bytes = Vec::new();
    response
        .take(read_limit)
        .read_to_end(&mut bytes)
        .context("read response body")?;
    if bytes.len() > max_bytes {
        bail!(
            "response too large: {} bytes (limit {})",
            bytes.len(),
            max_bytes
        );
    }

    let guessed_name = name_override.map(str::to_owned).or_else(|| {
        let before_query = url.split('?').next().unwrap_or(url);
        let last = before_query.rsplit('/').next()?.trim();
        if last.is_empty() {
            None
        } else {
            Some(last.to_owned())
        }
    });
    let mime = mime_override
        .map(str::to_owned)
        .or(header_mime)
        .unwrap_or_else(|| {
            guessed_name
                .as_deref()
                .map(|n| file_capability::infer_media_type(Path::new(n)))
                .unwrap_or("application/octet-stream")
                .to_string()
        });
    let fname = guessed_name.unwrap_or_else(|| "fetched".to_string());

    let size = bytes.len();
    let (change, file_id, import_id) = stage_byte_import(bytes.into(), &fname, &mime, tags, url)?;
    let content = content_handle_of(change.facts(), file_id).context("staged file content")?;
    pile.commit(collection, signer, change)
        .context("commit fetched file")?;
    ensure_files_after_commit(pile, collection, signer, runtime)?;
    out.line(format!(
        "{}  {}  ({})",
        handle_hex(content),
        file_capability::leaf_name(&fname),
        human_size(size as u64)
    ))?;
    if mime.starts_with("image/") {
        out.line(format!(
            "![{}](files:{})",
            file_capability::leaf_name(&fname),
            handle_hex(content)
        ))?;
    }
    out.line(format!("Import: {}", fmt_id(import_id)))
}

fn stage_byte_import(
    bytes: anybytes::Bytes,
    name: &str,
    mime: &str,
    tags: &[String],
    source: &str,
) -> Result<(Fragment, Id, Id)> {
    let mime = file_capability::normalize_media_type(mime)?;
    let mut change = file_capability::stage(bytes, name, &mime)?;
    let file_id = change.root().expect("staged file has a root");
    let import = entity! {
        metadata::tag: &KIND_IMPORT,
        file::root: &file_id,
        file::imported_at: now_tai()?,
        file::source_path: source.to_owned(),
        file::tag*: tags.iter().map(String::as_str),
    };
    let import_id = import.root().expect("import has a root");
    change += import;
    Ok((change, file_id, import_id))
}

fn cmd_list<P: TriblePattern>(
    space: &P,
    reader: &PileSnapshot,
    filter_tags: &[String],
    filter_mime: Option<&str>,
) -> Result<String> {
    let mut entries: Vec<(String, String, String, Vec<String>)> = Vec::new();

    for (eid, h) in find!(
        (eid: Id, h: FileHandle),
        pattern!(space, [{ ?eid @ metadata::tag: &KIND_FILE, file::content: ?h }])
    ) {
        let tags = tags_of(space, eid);
        if !filter_tags.is_empty() && !filter_tags.iter().all(|ft| tags.iter().any(|t| t == ft)) {
            continue;
        }
        let mime = read_mime(space, reader, eid)?.unwrap_or_else(|| "?".into());

        if let Some(mp) = filter_mime {
            if !mime.starts_with(mp) {
                continue;
            }
        }
        let fname = read_name(space, reader, eid)?.unwrap_or_else(|| "?".into());

        let hash = handle_hex(h);
        entries.push((hash, fname, mime, tags));
    }

    entries.sort_by(|a, b| a.1.cmp(&b.1));

    if entries.is_empty() {
        return Ok("(no files)\n".to_owned());
    }

    let mut output = String::new();
    for (hash, fname, mime, tags) in &entries {
        let tag_str = if tags.is_empty() {
            String::new()
        } else {
            format!("  [{}]", tags.join(", "))
        };
        writeln!(output, "{}  {}  {}{}", hash, fname, mime, tag_str)?;
    }

    Ok(output)
}

fn cmd_show<P: TriblePattern>(space: &P, reader: &PileSnapshot, id: &str) -> Result<String> {
    let eid = file_capability::resolve_selector(space, id)?;
    let mut output = String::new();

    if is_file(space, eid) {
        let h = content_handle_of(space, eid).unwrap();
        let size = reader
            .get::<anybytes::Bytes, _>(h)
            .context("read selected file size")?
            .len() as u64;
        writeln!(output, "Type:     file")?;
        writeln!(output, "Hash:     {}", handle_hex(h))?;
        writeln!(output, "Entity:   {}", fmt_id(eid))?;
        writeln!(
            output,
            "Name:     {}",
            read_name(space, reader, eid)?.unwrap_or("?".into())
        )?;
        writeln!(
            output,
            "MIME:     {}",
            read_mime(space, reader, eid)?.unwrap_or("?".into())
        )?;
        writeln!(output, "Size:     {}", human_size(size))?;
    } else if is_directory(space, eid) {
        let children = children_of(space, eid);
        writeln!(output, "Type:     directory")?;
        writeln!(output, "Entity:   {}", fmt_id(eid))?;
        writeln!(
            output,
            "Name:     {}",
            read_name(space, reader, eid)?.unwrap_or("?".into())
        )?;
        writeln!(output, "Children: {}", children.len())?;
    } else if is_import(space, eid) {
        let root = root_of(space, eid);
        let ts = imported_at_of(space, eid);
        let src = source_path_of(space, reader, eid)?;
        writeln!(output, "Type:     import")?;
        writeln!(output, "Entity:   {}", fmt_id(eid))?;
        if let Some(r) = root {
            writeln!(output, "Root:     {}", fmt_id(r))?;
        }
        if let Some(t) = ts {
            writeln!(output, "Imported: {}", format_date(t))?;
        }
        if let Some(s) = src {
            writeln!(output, "Source:   {s}")?;
        }
    } else {
        bail!("unknown entity kind for '{id}'");
    }

    let tags = tags_of(space, eid);
    if !tags.is_empty() {
        writeln!(output, "Tags:     {}", tags.join(", "))?;
    }

    Ok(output)
}

fn extract_tree<P: TriblePattern, R: BlobStoreGet>(
    space: &P,
    reader: &R,
    id: Id,
    dest: &Path,
    stats: &mut TreeStats,
    writes: &mut Vec<(PathBuf, Option<anybytes::Bytes>)>,
) -> Result<()> {
    if is_file(space, id) {
        let h =
            content_handle_of(space, id).ok_or_else(|| anyhow::anyhow!("no content for file"))?;
        let bytes: anybytes::Bytes = reader
            .get::<anybytes::Bytes, _>(h)
            .context("get extracted file blob")?;
        stats.files += 1;
        stats.bytes += bytes.len() as u64;
        writes.push((dest.to_owned(), Some(bytes)));
    } else if is_directory(space, id) {
        writes.push((dest.to_owned(), None));
        stats.dirs += 1;
        for cid in children_of(space, id) {
            let cname = file_capability::leaf_name(
                &read_name(space, reader, cid)?.unwrap_or_else(|| fmt_id(cid)),
            );
            extract_tree(space, reader, cid, &dest.join(&cname), stats, writes)?;
        }
    } else {
        bail!("unknown entity kind during extraction");
    }
    Ok(())
}

fn cmd_tag<P: TriblePattern>(
    pile: &mut FacultyStore,
    runtime: &tokio::runtime::Runtime,
    collection: Collection<SimpleArchive>,
    signer: &SigningKey,
    space: &P,
    reader: &FacultySnapshot,
    id: &str,
    tag_name: &str,
    out: &mut Out<'_>,
) -> Result<()> {
    let eid = file_capability::resolve_selector(space, id)?;

    let existing = tags_of(space, eid);
    if existing.iter().any(|t| t == tag_name) {
        out.line(format!("Tag '{tag_name}' already present."))?;
        return Ok(());
    }

    let name = runtime.block_on(read(pile, reader, |reader| {
        Ok(read_name(space, reader, eid)?.unwrap_or_else(|| fmt_id(eid)))
    }))?;
    let change = entity! { ExclusiveId::force_ref(&eid) @ file::tag: tag_name };
    pile.commit(collection, signer, change)
        .context("commit Files tag")?;
    ensure_files_after_commit(pile, collection, signer, runtime)?;

    out.line(format!("Tagged {name} with '{tag_name}'"))?;
    Ok(())
}

fn cmd_search<P: TriblePattern>(space: &P, reader: &PileSnapshot, query: &str) -> Result<String> {
    let needle = query.to_lowercase();
    let mut hits: Vec<(String, String, String, Vec<String>)> = Vec::new();

    for (eid, h) in find!(
        (eid: Id, h: FileHandle),
        pattern!(space, [{ ?eid @ metadata::tag: &KIND_FILE, file::content: ?h }])
    ) {
        let fname = read_name(space, reader, eid)?.unwrap_or_else(|| "?".into());
        let mime = read_mime(space, reader, eid)?.unwrap_or_else(|| "?".into());
        let tags = tags_of(space, eid);

        let fname_match = fname.to_lowercase().contains(&needle);
        let tag_match = tags.iter().any(|t| t.to_lowercase().contains(&needle));
        let mime_match = mime.to_lowercase().contains(&needle);

        if fname_match || tag_match || mime_match {
            hits.push((handle_hex(h), fname, mime, tags));
        }
    }

    hits.sort_by(|a, b| a.1.cmp(&b.1));

    if hits.is_empty() {
        return Ok(format!("No files matching '{query}'\n"));
    }

    let mut output = String::new();
    for (hash, fname, mime, tags) in &hits {
        let tag_str = if tags.is_empty() {
            String::new()
        } else {
            format!("  [{}]", tags.join(", "))
        };
        writeln!(output, "{}  {}  {}{}", hash, fname, mime, tag_str)?;
    }

    Ok(output)
}

fn cmd_imports<P: TriblePattern>(space: &P, reader: &PileSnapshot) -> Result<String> {
    let mut imports: Vec<(i128, Id, Option<String>, Vec<String>)> = Vec::new();

    for (eid,) in find!(
        (eid: Id),
        pattern!(space, [{ ?eid @ metadata::tag: &KIND_IMPORT }])
    ) {
        let ts = imported_at_of(space, eid).unwrap_or(0);
        let src = source_path_of(space, reader, eid)?;
        let tags = tags_of(space, eid);
        imports.push((ts, eid, src, tags));
    }

    imports.sort_by(|a, b| b.0.cmp(&a.0));

    if imports.is_empty() {
        return Ok("(no imports)\n".to_owned());
    }

    let mut output = String::new();
    for (ts, eid, src, tags) in &imports {
        let date = if *ts > 0 {
            format_date(*ts)
        } else {
            "?".into()
        };
        let src_str = src.as_deref().unwrap_or("?");
        let tag_str = if tags.is_empty() {
            String::new()
        } else {
            format!("  [{}]", tags.join(", "))
        };
        writeln!(
            output,
            "{}  {}  {}{}",
            &fmt_id(*eid)[..12],
            date,
            src_str,
            tag_str
        )?;
    }

    Ok(output)
}

fn cmd_tree<P: TriblePattern>(
    space: &P,
    reader: &PileSnapshot,
    id: &str,
    max_depth: Option<usize>,
) -> Result<String> {
    let eid = file_capability::resolve_selector(space, id)?;

    // If it's an import, follow to root.
    let root = if is_import(space, eid) {
        root_of(space, eid).ok_or_else(|| anyhow::anyhow!("import has no root"))?
    } else {
        eid
    };

    let mut output = String::new();
    print_tree(space, reader, root, "", "", max_depth, 0, &mut output)?;
    Ok(output)
}

fn print_tree<P: TriblePattern, R: BlobStoreGet>(
    space: &P,
    reader: &R,
    id: Id,
    prefix: &str,
    child_prefix: &str,
    max_depth: Option<usize>,
    depth: usize,
    output: &mut String,
) -> Result<()> {
    let name = read_name(space, reader, id)?.unwrap_or_else(|| fmt_id(id));

    if is_file(space, id) {
        let mime = read_mime(space, reader, id)?.unwrap_or_else(|| "?".into());
        let size_str = content_handle_of(space, id)
            .map(|h| reader.get::<anybytes::Bytes, _>(h))
            .transpose()
            .context("read displayed file size")?
            .map(|b| human_size(b.len() as u64))
            .unwrap_or_else(|| "?".into());
        writeln!(output, "{prefix}{name}  ({mime}, {size_str})")?;
    } else if is_directory(space, id) {
        let children = children_of(space, id);
        if max_depth.is_some_and(|d| depth >= d) {
            writeln!(output, "{prefix}{name}/  ({} children)", children.len())?;
            return Ok(());
        }
        writeln!(output, "{prefix}{name}/")?;
        let mut dirs: Vec<(String, Id)> = Vec::new();
        let mut files: Vec<(String, Id)> = Vec::new();
        for &cid in &children {
            let cname = read_name(space, reader, cid)?.unwrap_or_else(|| fmt_id(cid));
            if is_directory(space, cid) {
                dirs.push((cname, cid));
            } else {
                files.push((cname, cid));
            }
        }
        dirs.sort_by(|a, b| a.0.cmp(&b.0));
        files.sort_by(|a, b| a.0.cmp(&b.0));

        let all: Vec<Id> = dirs.iter().chain(files.iter()).map(|(_, id)| *id).collect();
        for (i, &cid) in all.iter().enumerate() {
            let last = i == all.len() - 1;
            let connector = if last { "└── " } else { "├── " };
            let continuation = if last { "    " } else { "│   " };
            print_tree(
                space,
                reader,
                cid,
                &format!("{child_prefix}{connector}"),
                &format!("{child_prefix}{continuation}"),
                max_depth,
                depth + 1,
                output,
            )?;
        }
    } else {
        writeln!(output, "{prefix}{name}  (unknown)")?;
    }
    Ok(())
}

fn cmd_diff<P: TriblePattern>(
    space: &P,
    reader: &PileSnapshot,
    left_id: &str,
    right_id: &str,
) -> Result<String> {
    let resolve_root = |raw: &str| -> Result<Id> {
        let eid = file_capability::resolve_selector(space, raw)?;
        if is_import(space, eid) {
            root_of(space, eid).ok_or_else(|| anyhow::anyhow!("import has no root"))
        } else {
            Ok(eid)
        }
    };

    let left = resolve_root(left_id)?;
    let right = resolve_root(right_id)?;

    if left == right {
        return Ok("Identical (same entity).\n".to_owned());
    }

    let mut stats = DiffStats::default();
    let mut output = String::new();
    diff_tree(space, reader, left, right, "", &mut stats, &mut output)?;

    if stats.is_empty() {
        writeln!(output, "No differences.")?;
    } else {
        writeln!(
            output,
            "\n{} added, {} removed, {} modified",
            stats.added, stats.removed, stats.modified,
        )?;
    }
    Ok(output)
}

#[derive(Default)]
struct DiffStats {
    added: usize,
    removed: usize,
    modified: usize,
}

impl DiffStats {
    fn is_empty(&self) -> bool {
        self.added == 0 && self.removed == 0 && self.modified == 0
    }
}

fn diff_tree<P: TriblePattern, R: BlobStoreGet>(
    space: &P,
    reader: &R,
    left: Id,
    right: Id,
    path: &str,
    stats: &mut DiffStats,
    output: &mut String,
) -> Result<()> {
    // Merkle shortcut: same id means identical subtree.
    if left == right {
        return Ok(());
    }

    let left_is_dir = is_directory(space, left);
    let right_is_dir = is_directory(space, right);

    // Both files — content changed.
    if !left_is_dir && !right_is_dir {
        let lname = read_name(space, reader, left)?.unwrap_or_else(|| "?".into());
        let lsize = file_size(space, reader, left)?;
        let rsize = file_size(space, reader, right)?;
        writeln!(
            output,
            "  ~ {path}{lname}  ({} → {})",
            human_size(lsize),
            human_size(rsize)
        )?;
        stats.modified += 1;
        return Ok(());
    }

    // Type mismatch: show as remove + add.
    if left_is_dir != right_is_dir {
        print_diff_removed(space, reader, left, path, stats, output)?;
        print_diff_added(space, reader, right, path, stats, output)?;
        return Ok(());
    }

    // Both directories — diff children by name.
    let left_children = named_children(space, reader, left)?;
    let right_children = named_children(space, reader, right)?;

    let left_name = read_name(space, reader, left)?.unwrap_or_else(|| "?".into());
    let sub = if path.is_empty() {
        format!("{left_name}/")
    } else {
        format!("{path}{left_name}/")
    };

    let mut li = left_children.iter().peekable();
    let mut ri = right_children.iter().peekable();

    // Merge-join on name (BTreeMap is sorted).
    loop {
        match (li.peek(), ri.peek()) {
            (None, None) => break,
            (Some(_), None) => {
                let (_lname, lid) = li.next().unwrap();
                print_diff_removed(space, reader, *lid, &sub, stats, output)?;
            }
            (None, Some(_)) => {
                let (_rname, rid) = ri.next().unwrap();
                print_diff_added(space, reader, *rid, &sub, stats, output)?;
            }
            (Some((lname, _)), Some((rname, _))) => match lname.cmp(rname) {
                std::cmp::Ordering::Less => {
                    let (lname, lid) = li.next().unwrap();
                    print_diff_removed(space, reader, *lid, &sub, stats, output)?;
                    let _ = lname;
                }
                std::cmp::Ordering::Greater => {
                    let (rname, rid) = ri.next().unwrap();
                    print_diff_added(space, reader, *rid, &sub, stats, output)?;
                    let _ = rname;
                }
                std::cmp::Ordering::Equal => {
                    let (_lname, lid) = li.next().unwrap();
                    let (_rname, rid) = ri.next().unwrap();
                    diff_tree(space, reader, *lid, *rid, &sub, stats, output)?;
                }
            },
        }
    }
    Ok(())
}

fn named_children<P: TriblePattern, R: BlobStoreGet>(
    space: &P,
    reader: &R,
    id: Id,
) -> Result<BTreeMap<String, Id>> {
    let mut map = BTreeMap::new();
    for cid in children_of(space, id) {
        let name = read_name(space, reader, cid)?.unwrap_or_else(|| fmt_id(cid));
        map.insert(name, cid);
    }
    Ok(map)
}

fn file_size<P: TriblePattern, R: BlobStoreGet>(space: &P, reader: &R, id: Id) -> Result<u64> {
    Ok(content_handle_of(space, id)
        .map(|h| reader.get::<anybytes::Bytes, _>(h))
        .transpose()
        .context("read compared file size")?
        .map(|b| b.len() as u64)
        .unwrap_or(0))
}

fn print_diff_added<P: TriblePattern, R: BlobStoreGet>(
    space: &P,
    reader: &R,
    id: Id,
    path: &str,
    stats: &mut DiffStats,
    output: &mut String,
) -> Result<()> {
    let name = read_name(space, reader, id)?.unwrap_or_else(|| "?".into());
    if is_directory(space, id) {
        writeln!(output, "  + {path}{name}/")?;
        stats.added += 1;
        let sub = format!("{path}{name}/");
        for cid in children_of(space, id) {
            print_diff_added(space, reader, cid, &sub, stats, output)?;
        }
    } else {
        let size = file_size(space, reader, id)?;
        writeln!(output, "  + {path}{name}  ({})", human_size(size))?;
        stats.added += 1;
    }
    Ok(())
}

fn print_diff_removed<P: TriblePattern, R: BlobStoreGet>(
    space: &P,
    reader: &R,
    id: Id,
    path: &str,
    stats: &mut DiffStats,
    output: &mut String,
) -> Result<()> {
    let name = read_name(space, reader, id)?.unwrap_or_else(|| "?".into());
    if is_directory(space, id) {
        writeln!(output, "  - {path}{name}/")?;
        stats.removed += 1;
        let sub = format!("{path}{name}/");
        for cid in children_of(space, id) {
            print_diff_removed(space, reader, cid, &sub, stats, output)?;
        }
    } else {
        let size = file_size(space, reader, id)?;
        writeln!(output, "  - {path}{name}  ({})", human_size(size))?;
        stats.removed += 1;
    }
    Ok(())
}

// ── main ─────────────────────────────────────────────────────────────────

/// The Files content the WeMM index reads.
#[cfg(feature = "wemm")]
fn content_attributes() -> [Id; 1] {
    [file::content.id()]
}

/// Every content whose cosine is at least `floor`, with every entity that
/// holds it under `file::content` and carries every one of `tags`: one query
/// over the index's cosines and the Files facts. A mail attachment saved three
/// times is three holders of one content and one hit.
#[cfg(feature = "wemm")]
fn similar_contents<P: TriblePattern>(
    space: &P,
    cosines: &ReconstructedCosines,
    floor: f64,
    tags: &[Inline<inlineencodings::ShortString>],
) -> BTreeMap<FileHandle, std::collections::BTreeSet<Id>> {
    use triblespace::core::query::Constraint;

    let tag_attribute: Inline<inlineencodings::GenId> = file::tag.id().to_inline();
    let mut holders: BTreeMap<FileHandle, std::collections::BTreeSet<Id>> = BTreeMap::new();
    for (content, holder) in find!(
        (content: FileHandle, holder: Id),
        and!(
            cosines.similar_to::<inlineencodings::Handle<blobencodings::RawBytes>>(content, floor),
            pattern!(space, [{ ?holder @ file::content: ?content }]),
            IntersectionConstraint::new(
                tags.iter()
                    .map(|tag| {
                        Box::new(space.pattern(holder, tag_attribute, *tag))
                            as Box<dyn Constraint + Send + Sync>
                    })
                    .collect()
            ),
        )
    ) {
        holders.entry(content).or_default().insert(holder);
    }
    holders
}

/// `files similar`: Files contents ranked by WeMM cosine to a text query, or
/// to a stored file's content read the way the index reads it (an image
/// whole, the text of text, HTML or a PDF). The cosine is reconstructed from
/// NVFP4 rows and is not a calibrated relevance; the ranking and the limit are
/// presentation of the `find!` answer, and a file query leaves out its own
/// content.
#[cfg(feature = "wemm")]
fn cmd_similar<P: TriblePattern>(
    store: &mut FacultyStore,
    collection: Collection<SimpleArchive>,
    runtime: &std::sync::Arc<tokio::runtime::Runtime>,
    space: &P,
    snapshot: &FacultySnapshot,
    session: &crate::wemm::Session,
    options: &SimilarityOptions<'_>,
    out: &mut Out<'_>,
) -> Result<()> {
    use crate::wemm::Query;

    let reader = AcquiringReader::new(snapshot.clone(), runtime.clone());
    // A tag longer than a short string is carried by no file.
    let tags: Vec<Inline<inlineencodings::ShortString>> = options
        .tags
        .iter()
        .map(|tag| {
            tag.as_str()
                .try_to_inline()
                .map_err(|_| anyhow::anyhow!("{tag:?} cannot be a Files tag"))
        })
        .collect::<Result<_>>()?;
    let attributes = content_attributes();
    let (cosines, own, label) = match (options.text, options.id) {
        (Some(text), None) => (
            session.cosines(store, collection, &attributes, runtime, Query::Text(text))?,
            None,
            format!("{text:?}"),
        ),
        (None, Some(selector)) => {
            let entity = file_capability::resolve_selector(space, selector)?;
            let content = content_handle_of(space, entity).ok_or_else(|| {
                anyhow::anyhow!("that entity has no content; query with --text instead")
            })?;
            let bytes: anybytes::Bytes =
                reader.get(content).context("read the query file's bytes")?;
            let name = read_name(space, &reader, entity)?.unwrap_or_else(|| "?".into());
            let query = Query::Content(bytes.as_ref());
            (
                session.cosines(store, collection, &attributes, runtime, query)?,
                Some(content),
                name,
            )
        }
        _ => bail!("give a file id/hash, or --text \"a query\""),
    };
    anyhow::ensure!(
        !cosines.is_empty(),
        "the Files WeMM index has no rows yet; run `files index`"
    );
    let holders = similar_contents(
        space,
        &cosines,
        options.floor.unwrap_or(f64::NEG_INFINITY),
        &tags,
    );
    let mut hits: Vec<(f64, FileHandle)> = holders
        .keys()
        .filter(|content| Some(**content) != own)
        .map(|content| {
            let cosine = cosines
                .cosine(content)
                .expect("a joined content has a score");
            (cosine, *content)
        })
        .collect();
    hits.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    hits.truncate(options.limit);
    if hits.is_empty() {
        out.line(format!("no files similar to {label}"))?;
        return Ok(());
    }
    out.line(format!(
        "Similar to {label} (WeMM reconstructed cosine, not a calibrated relevance):"
    ))?;
    for (cos, content) in hits {
        let held = &holders[&content];
        let eid = *held.first().expect("every hit has a holder");
        let name = read_name(space, &reader, eid)?.unwrap_or_else(|| "?".into());
        let mime = read_mime(space, &reader, eid)?.unwrap_or_else(|| "?".into());
        let hash = handle_hex(content);
        let tags = tags_of(space, eid);
        let tagstr = if tags.is_empty() {
            String::new()
        } else {
            format!("  [{}]", tags.join(", "))
        };
        let others = if held.len() > 1 {
            format!("  (+{} more holding it)", held.len() - 1)
        } else {
            String::new()
        };
        out.line(format!(
            "  {cos:.3}  {name}  ({mime})  {hash}{tagstr}{others}"
        ))?;
    }
    Ok(())
}

/// A configured Files capability. Constructing it performs no I/O; each operation
/// observes one fresh maintained collection view through its storage handle.
#[derive(Clone, Debug)]
pub struct Files {
    storage: Storage,
}

#[derive(Clone, Debug)]
pub struct Export {
    pub bytes: anybytes::Bytes,
    pub content: FileHandle,
}

impl Export {
    pub fn uri(&self) -> String {
        format!("files:{}", handle_hex(self.content))
    }
}

#[derive(Clone, Debug)]
pub struct StoredView {
    pub bytes: anybytes::Bytes,
    pub mime_type: String,
}

/// A fully acquired extraction plan. No filesystem write occurs while payload
/// acquisition may retry. Paths are explicit; stdout conventions do not exist here.
#[derive(Debug)]
pub struct Extraction {
    pub destination: PathBuf,
    writes: Vec<(PathBuf, Option<anybytes::Bytes>)>,
}

impl Extraction {
    pub fn write(self) -> Result<()> {
        for (path, bytes) in self.writes {
            match bytes {
                Some(bytes) => fs::write(&path, bytes.as_ref())
                    .with_context(|| format!("write {}", path.display()))?,
                None => fs::create_dir_all(&path)
                    .with_context(|| format!("mkdir {}", path.display()))?,
            }
        }
        Ok(())
    }
}

pub struct FetchOptions<'a> {
    pub url: &'a str,
    pub mime: Option<&'a str>,
    pub name: Option<&'a str>,
    pub tags: &'a [String],
    pub max_bytes: usize,
}

pub struct SimilarityOptions<'a> {
    pub id: Option<&'a str>,
    pub text: Option<&'a str>,
    /// Only contents whose reconstructed cosine is at least this, in [-1, 1].
    /// No threshold is calibrated across modalities; none ranks every content.
    pub floor: Option<f64>,
    pub limit: usize,
    pub tags: &'a [String],
}

impl SimilarityOptions<'_> {
    fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.id.is_some() ^ self.text.is_some(),
            "provide exactly one of id or text"
        );
        if let Some(floor) = self.floor {
            anyhow::ensure!(
                floor.is_finite() && (-1.0..=1.0).contains(&floor),
                "floor must be a cosine between -1 and 1"
            );
        }
        Ok(())
    }
}

impl Files {
    pub fn new(pile: PathBuf, key: Option<PathBuf>) -> Self {
        Self::with_storage(Storage::new(pile, key))
    }

    pub fn with_storage(storage: Storage) -> Self {
        Self { storage }
    }

    /// Original payload only: neither MIME nor filename is needed for export.
    pub fn get(&self, id: &str) -> Result<Export> {
        with_files_view(&self.storage, |store, _, _, facts, snapshot, rt| {
            load_export(store, rt, facts, snapshot, id)
        })
    }

    pub fn view(
        &self,
        id: &str,
        options: &super::presentation::ViewOptions,
    ) -> Result<crate::out::Part> {
        options.validate()?;
        let selected = with_files_view(&self.storage, |store, _, _, facts, snapshot, rt| {
            load_view(store, rt, facts, snapshot, id)
        })?;
        super::presentation::present(selected.bytes, &selected.mime_type, options)
    }

    pub fn extract(&self, id: &str, destination: Option<&Path>) -> Result<Extraction> {
        with_files_view(&self.storage, |store, _, _, facts, snapshot, rt| {
            prepare_extraction(store, rt, facts, snapshot, id, destination)
        })
    }

    pub fn add_path(
        &self,
        path: &Path,
        mime: Option<&str>,
        tags: &[String],
        dry_run: bool,
        out: &mut Out<'_>,
    ) -> Result<()> {
        if dry_run {
            return cmd_add_dry_run(path, tags, out);
        }
        with_files_store(&self.storage, |store, collection, signer, runtime| {
            cmd_add(store, collection, signer, runtime, path, mime, tags, out)
        })
    }

    /// Import resident bytes without manufacturing a temporary filesystem path.
    /// Returns the intrinsic file id; import provenance is exhaust of publication.
    pub fn add_bytes(
        &self,
        bytes: anybytes::Bytes,
        name: &str,
        mime: &str,
        tags: &[String],
    ) -> Result<Id> {
        let (change, file_id, _) = stage_byte_import(bytes, name, mime, tags, "resident bytes")?;
        with_files_store(&self.storage, |store, collection, signer, runtime| {
            store
                .commit(collection, signer, change)
                .context("commit Files byte import")?;
            ensure_files_after_commit(store, collection, signer, runtime)?;
            Ok(file_id)
        })
    }

    pub fn list(&self, tags: &[String], mime: Option<&str>) -> Result<String> {
        with_files_view(&self.storage, |store, _, _, facts, snapshot, rt| {
            rt.block_on(read(store, snapshot, |reader| {
                cmd_list(facts, reader, tags, mime)
            }))
        })
    }

    pub fn show(&self, id: &str) -> Result<String> {
        with_files_view(&self.storage, |store, _, _, facts, snapshot, rt| {
            rt.block_on(read(store, snapshot, |reader| cmd_show(facts, reader, id)))
        })
    }

    pub fn tag(&self, id: &str, name: &str, out: &mut Out<'_>) -> Result<()> {
        with_files_view(
            &self.storage,
            |store, collection, signer, facts, snapshot, rt| {
                cmd_tag(
                    store, rt, collection, signer, facts, snapshot, id, name, out,
                )
            },
        )
    }

    pub fn fetch(&self, options: &FetchOptions<'_>, out: &mut Out<'_>) -> Result<()> {
        anyhow::ensure!(options.max_bytes > 0, "max_bytes must be positive");
        with_files_store(&self.storage, |store, collection, signer, runtime| {
            cmd_fetch(
                store,
                collection,
                signer,
                runtime,
                options.url,
                options.mime,
                options.name,
                options.tags,
                options.max_bytes,
                out,
            )
        })
    }

    pub fn search(&self, query: &str) -> Result<String> {
        with_files_view(&self.storage, |store, _, _, facts, snapshot, rt| {
            rt.block_on(read(store, snapshot, |reader| {
                cmd_search(facts, reader, query)
            }))
        })
    }

    /// Rank stored files by meaning (see [`cmd_similar`]), binding the WeMM
    /// model the environment names.
    #[cfg(feature = "wemm")]
    pub fn similar(&self, options: &SimilarityOptions<'_>, out: &mut Out<'_>) -> Result<()> {
        options.validate()?;
        self.similar_with(
            &crate::wemm::Session::from_env(self.storage.path())?,
            options,
            out,
        )
    }

    /// [`Self::similar`] with a model already bound.
    #[cfg(feature = "wemm")]
    pub fn similar_with(
        &self,
        session: &crate::wemm::Session,
        options: &SimilarityOptions<'_>,
        out: &mut Out<'_>,
    ) -> Result<()> {
        options.validate()?;
        with_files_view(
            &self.storage,
            |store, collection, _, facts, snapshot, runtime| {
                cmd_similar(
                    store, collection, runtime, facts, snapshot, session, options, out,
                )
            },
        )
    }

    #[cfg(not(feature = "wemm"))]
    pub fn similar(&self, options: &SimilarityOptions<'_>, _out: &mut Out<'_>) -> Result<()> {
        options.validate()?;
        crate::wemm_unavailable()
    }

    /// Derive the WeMM index over every stored file's content, whoever saved
    /// it, binding the model the environment names. Saving never embeds.
    #[cfg(feature = "wemm")]
    pub fn index(&self, out: &mut Out<'_>) -> Result<()> {
        self.index_with(&crate::wemm::Session::from_env(self.storage.path())?, out)
    }

    /// [`Self::index`] with a model already bound.
    #[cfg(feature = "wemm")]
    pub fn index_with(&self, session: &crate::wemm::Session, out: &mut Out<'_>) -> Result<()> {
        with_files_store(&self.storage, |store, collection, signer, runtime| {
            session.index(
                store,
                collection,
                &content_attributes(),
                signer,
                runtime,
                out,
            )
        })
    }

    #[cfg(not(feature = "wemm"))]
    pub fn index(&self, _out: &mut Out<'_>) -> Result<()> {
        crate::wemm_unavailable()
    }

    pub fn imports(&self) -> Result<String> {
        with_files_view(&self.storage, |store, _, _, facts, snapshot, rt| {
            rt.block_on(read(store, snapshot, |reader| cmd_imports(facts, reader)))
        })
    }

    pub fn tree(&self, id: &str, depth: Option<usize>) -> Result<String> {
        with_files_view(&self.storage, |store, _, _, facts, snapshot, rt| {
            rt.block_on(read(store, snapshot, |reader| {
                cmd_tree(facts, reader, id, depth)
            }))
        })
    }

    /// Resolve a batch in one view. Individual misses remain individual results;
    /// no selector is interpreted as an instruction to read a host file or stdin.
    pub fn resolve(
        &self,
        selectors: &[String],
    ) -> Result<Vec<Result<file_capability::FileReference>>> {
        with_files_view(&self.storage, |_, _, _, facts, _, _| {
            Ok(selectors
                .iter()
                .map(|selector| file_capability::resolve_reference(facts, selector))
                .collect())
        })
    }

    pub fn diff(&self, left: &str, right: &str) -> Result<String> {
        with_files_view(&self.storage, |store, _, _, facts, snapshot, rt| {
            rt.block_on(read(store, snapshot, |reader| {
                cmd_diff(facts, reader, left, right)
            }))
        })
    }
}

fn selected_target<P: TriblePattern>(space: &P, id: &str) -> Result<Id> {
    let eid = file_capability::resolve_selector(space, id)?;
    if is_import(space, eid) {
        root_of(space, eid).ok_or_else(|| anyhow::anyhow!("import has no root"))
    } else {
        Ok(eid)
    }
}

fn load_export<P, S>(
    store: &mut S,
    rt: &tokio::runtime::Runtime,
    space: &P,
    reader: &S::Snapshot,
    id: &str,
) -> Result<Export>
where
    P: TriblePattern,
    S: SnapshotSource + AsyncBlobStoreAcquire,
    S::Snapshot: BlobStoreGet + BlobStoreList,
{
    let content = match file_capability::resolve_reference(space, id)? {
        file_capability::FileReference::Content(content) => content,
        file_capability::FileReference::Entity(_) => {
            let target = selected_target(space, id)?;
            anyhow::ensure!(
                is_file(space, target),
                "get requires a file; use CLI extraction for directories"
            );
            content_handle_of(space, target).context("no content for file")?
        }
    };
    let bytes = rt.block_on(read(store, reader, |reader| {
        reader
            .get::<anybytes::Bytes, _>(content)
            .context("get file blob")
    }))?;
    Ok(Export { bytes, content })
}

fn load_view<P, S>(
    store: &mut S,
    rt: &tokio::runtime::Runtime,
    space: &P,
    reader: &S::Snapshot,
    id: &str,
) -> Result<StoredView>
where
    P: TriblePattern,
    S: SnapshotSource + AsyncBlobStoreAcquire,
    S::Snapshot: BlobStoreGet + BlobStoreList,
{
    let target = selected_target(space, id)?;
    anyhow::ensure!(
        is_file(space, target),
        "view requires a file; use CLI get to extract a directory"
    );
    let content = content_handle_of(space, target).context("no content for file")?;
    rt.block_on(read(store, reader, |reader| {
        let mime_type =
            read_mime(space, reader, target)?.unwrap_or_else(|| "application/octet-stream".into());
        anyhow::ensure!(
            mime_type.starts_with("text/")
                || mime_type.starts_with("image/")
                || mime_type.starts_with("audio/"),
            "cannot present MIME type {mime_type:?}; use files get {id} to extract it",
        );
        let bytes = reader
            .get::<anybytes::Bytes, _>(content)
            .context("read file content")?;
        Ok(StoredView { bytes, mime_type })
    }))
}

fn prepare_extraction<P, S>(
    store: &mut S,
    rt: &tokio::runtime::Runtime,
    space: &P,
    reader: &S::Snapshot,
    id: &str,
    destination: Option<&Path>,
) -> Result<Extraction>
where
    P: TriblePattern,
    S: SnapshotSource + AsyncBlobStoreAcquire,
    S::Snapshot: BlobStoreGet + BlobStoreList,
{
    let target = selected_target(space, id)?;
    rt.block_on(read(store, reader, |reader| {
        let destination = match destination {
            Some(path) => path.to_owned(),
            None => PathBuf::from(file_capability::leaf_name(
                &read_name(space, reader, target)?.unwrap_or_else(|| "extracted".into()),
            )),
        };
        let mut stats = TreeStats {
            files: 0,
            dirs: 0,
            bytes: 0,
        };
        let mut writes = Vec::new();
        extract_tree(space, reader, target, &destination, &mut stats, &mut writes)?;
        Ok(Extraction {
            destination,
            writes,
        })
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{initialize_signer, load_signer, runtime};
    #[cfg(feature = "local-embed")]
    use ed25519_dalek::SigningKey;
    use std::collections::BTreeSet;
    use std::sync::atomic::{AtomicU64, Ordering};
    use triblespace::core::blob::encodings::UnknownBlob;
    use triblespace::core::repo::{BlobStoreList, MissingBlob, WantRead};

    static NEXT_TEST_PILE: AtomicU64 = AtomicU64::new(0);

    #[cfg(feature = "local-embed")]
    const WORDPIECE: &str = r###"{
      "added_tokens": [],
      "normalizer": {"type": "BertNormalizer", "clean_text": true,
                     "handle_chinese_chars": true, "strip_accents": null,
                     "lowercase": true},
      "pre_tokenizer": {"type": "BertPreTokenizer"},
      "decoder": {"type": "WordPiece", "prefix": "##", "cleanup": true},
      "model": {"type": "WordPiece", "unk_token": "[UNK]",
                "continuing_subword_prefix": "##",
                "max_input_chars_per_word": 100,
                "vocab": {"[UNK]": 0, "hello": 1}}
    }"###;

    #[cfg(feature = "local-embed")]
    const LATER_WORDPIECE: &str = r###"{
      "added_tokens": [],
      "normalizer": {"type": "BertNormalizer", "clean_text": true,
                     "handle_chinese_chars": true, "strip_accents": null,
                     "lowercase": true},
      "pre_tokenizer": {"type": "BertPreTokenizer"},
      "decoder": {"type": "WordPiece", "prefix": "##", "cleanup": true},
      "model": {"type": "WordPiece", "unk_token": "[UNK]",
                "continuing_subword_prefix": "##",
                "max_input_chars_per_word": 100,
                "vocab": {"[UNK]": 0, "later": 1}}
    }"###;

    struct TestPile {
        dir: PathBuf,
        path: PathBuf,
    }

    impl TestPile {
        fn new() -> Self {
            let nonce = NEXT_TEST_PILE.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "faculties-files-selector-{}-{nonce}",
                std::process::id()
            ));
            fs::create_dir_all(&dir).unwrap();
            let path = dir.join("test.pile");
            fs::File::create(&path).unwrap();
            initialize_signer(&path, None).unwrap();
            Self { dir, path }
        }
    }

    impl Drop for TestPile {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn import_and_tag_advance_the_view_a_reader_prepares() {
        let fixture = TestPile::new();
        let storage = Storage::new(fixture.path.clone(), None);
        let (succinct, rank9) = storage
            .with_pile(|pile, signer| {
                let source = open(pile, DEFAULT_SCOPE_ID, signer.verifying_key())?;
                let succinct = pile.attach::<SuccinctArchiveBlob>(source, ())?;
                Ok((
                    succinct,
                    pile.attach::<Rank9AcceleratedSuccinctArchiveBlob>(source, succinct)?,
                ))
            })
            .unwrap();
        let observe = || {
            storage
                .with_pile(|pile, signer| {
                    let snapshot = pollster::block_on(async {
                        drop(pile.maintain_attached(succinct, signer).await?);
                        pile.maintain_attached(rank9, signer).await
                    })?;
                    Ok(snapshot.read_facts(rank9)?)
                })
                .unwrap()
        };
        let faculty = Files::with_storage(storage.clone());
        let id = faculty
            .add_bytes(
                anybytes::Bytes::from_source(b"eager file".to_vec()),
                "eager.txt",
                "text/plain",
                &[],
            )
            .unwrap();
        let old_facts = observe();
        assert!(content_handle_of(&old_facts, id).is_some());
        let mut discard = |_| Ok(());
        faculty
            .tag(&format!("{id:x}"), "eager", &mut Out::new(&mut discard))
            .unwrap();
        let facts = observe();
        assert!(tags_of(&facts, id).contains(&"eager".to_owned()));
        assert!(!tags_of(&old_facts, id).contains(&"eager".to_owned()));
    }

    /// A gb10 build used to maintain the semantic indexes inside `files add`;
    /// on this pile, which holds no model, that attempt printed its failure.
    #[test]
    fn saving_a_file_never_embeds() {
        let fixture = TestPile::new();
        let files = Files::with_storage(Storage::new(fixture.path.clone(), None));
        let note = fixture.dir.join("note.txt");
        fs::write(&note, "a saved note").unwrap();
        let mut printed = String::new();
        let mut emit = |part: crate::out::Part| {
            if let crate::out::Part::Text { text } = part {
                printed.push_str(&text);
            }
            Ok(())
        };
        files
            .add_path(&note, None, &[], false, &mut Out::new(&mut emit))
            .unwrap();
        assert!(printed.contains("note.txt"), "{printed}");
        assert!(!printed.contains("Semantic"), "{printed}");
    }

    struct AcquiringPile {
        pile: Pile,
        remote: PileSnapshot,
        requests: Vec<[u8; 32]>,
        unavailable: Option<[u8; 32]>,
    }

    impl SnapshotSource for AcquiringPile {
        type Snapshot = PileSnapshot;
        type SnapshotError = <Pile as SnapshotSource>::SnapshotError;

        fn snapshot(&mut self) -> Result<PileSnapshot, Self::SnapshotError> {
            self.pile.snapshot()
        }
    }

    impl AsyncBlobStoreAcquire for AcquiringPile {
        type AcquireError = std::convert::Infallible;

        fn acquire(
            &mut self,
            handle: Inline<inlineencodings::Handle<UnknownBlob>>,
        ) -> impl std::future::Future<Output = Result<Option<anybytes::Bytes>, Self::AcquireError>> + Send
        {
            let local = self.pile.snapshot().unwrap();
            let bytes = if local.contains_blob(handle).unwrap() {
                Some(local.get::<anybytes::Bytes, UnknownBlob>(handle).unwrap())
            } else {
                self.requests.push(handle.raw);
                if self.unavailable == Some(handle.raw) {
                    None
                } else {
                    self.remote.get::<anybytes::Bytes, UnknownBlob>(handle).ok()
                }
            };
            if let Some(bytes) = &bytes {
                assert_eq!(
                    self.pile.put::<UnknownBlob, _>(bytes.clone()).unwrap().raw,
                    handle.raw
                );
            }
            std::future::ready(Ok(bytes))
        }
    }

    fn sparse_store(fragment: Fragment) -> (TestPile, TestPile, TribleSet, AcquiringPile) {
        let source = TestPile::new();
        let target = TestPile::new();
        let facts = fragment.facts().clone();
        let signer = load_signer(&source.path, None).unwrap();
        let mut pile = Pile::open(&source.path).unwrap();
        let collection = open(&mut pile, DEFAULT_SCOPE_ID, signer.verifying_key()).unwrap();
        pile.commit(collection, &signer, fragment).unwrap();
        let remote = pile.snapshot().unwrap();
        pile.close().unwrap();
        let store = AcquiringPile {
            pile: Pile::open(&target.path).unwrap(),
            remote,
            requests: Vec::new(),
            unavailable: None,
        };
        (source, target, facts, store)
    }

    #[test]
    fn resident_raw_export_does_not_acquire_unavailable_mime_metadata() {
        let bytes = vec![0_u8, 255, 10, 128];
        let selected = file_capability::stage(bytes.clone(), "selected.png", "image/png").unwrap();
        let id = selected.root().unwrap();
        let content = content_handle_of(selected.facts(), id).unwrap();
        let mime = file_capability::media_type_name_handle(selected.facts(), id).unwrap();
        let (_source, _target, facts, mut store) = sparse_store(selected);
        store
            .pile
            .put::<blobencodings::RawBytes, _>(bytes.clone())
            .unwrap();
        store.unavailable = Some(mime.raw);
        let snapshot = store.snapshot().unwrap();
        let exported = load_export(
            &mut store,
            &runtime().unwrap(),
            &facts,
            &snapshot,
            &format!("{id:x}"),
        )
        .unwrap();
        assert_eq!(exported.bytes.as_ref(), bytes);
        assert_eq!(exported.uri(), format!("files:{}", handle_hex(content)));
        assert!(store.requests.is_empty());
        assert!(!store.snapshot().unwrap().contains_blob(mime).unwrap());
        store.pile.close().unwrap();
    }

    #[test]
    fn lazy_view_fetches_only_selected_mime_and_payload() {
        for (mime, bytes) in [
            ("text/plain", b"one\ntwo".as_slice()),
            ("text/plain", b"".as_slice()),
            ("image/png", &[0_u8, 255, 10][..]),
            ("audio/wav", &[255_u8, 0, 1][..]),
        ] {
            let selected = file_capability::stage(bytes.to_vec(), "selected", mime).unwrap();
            let id = selected.root().unwrap();
            let content = content_handle_of(selected.facts(), id).unwrap();
            let mime_handle =
                file_capability::media_type_name_handle(selected.facts(), id).unwrap();
            let unrelated =
                file_capability::stage(b"unrelated".to_vec(), "other", "text/plain").unwrap();
            let (_source, _target, facts, mut store) = sparse_store(selected + unrelated);
            let snapshot = store.snapshot().unwrap();
            let selected = load_view(
                &mut store,
                &runtime().unwrap(),
                &facts,
                &snapshot,
                &format!("{id:x}"),
            )
            .unwrap();
            assert_eq!(store.requests, [mime_handle.raw, content.raw]);
            assert!(!snapshot.contains_blob(content).unwrap());
            store.pile.close().unwrap();
            assert_eq!(selected.mime_type, mime);
            assert_eq!(selected.bytes.as_ref(), bytes);
        }
    }

    #[test]
    fn unsupported_display_fetches_mime_but_not_payload() {
        let selected =
            file_capability::stage(b"%PDF".to_vec(), "document.pdf", "application/pdf").unwrap();
        let id = selected.root().unwrap();
        let mime = file_capability::media_type_name_handle(selected.facts(), id).unwrap();
        let (_source, _target, facts, mut store) = sparse_store(selected);
        let snapshot = store.snapshot().unwrap();
        let error = load_view(
            &mut store,
            &runtime().unwrap(),
            &facts,
            &snapshot,
            &format!("{id:x}"),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("use files get"), "{error:#}");
        assert_eq!(store.requests, [mime.raw]);
        store.pile.close().unwrap();
    }

    #[test]
    fn lazy_configured_descriptor_reads_only_its_descriptor_and_name() {
        let source = TestPile::new();
        let target = TestPile::new();
        let signer = load_signer(&source.path, None).unwrap();
        let mut pile = Pile::open(&source.path).unwrap();
        let collection = open(&mut pile, DEFAULT_SCOPE_ID, signer.verifying_key()).unwrap();
        let remote = pile.snapshot().unwrap();
        let descriptor: TribleSet = remote.get(collection.handle()).unwrap();
        let name = triblespace::core::collection::descriptor::name(&descriptor)
            .unwrap()
            .unwrap();
        pile.close().unwrap();
        let mut store = AcquiringPile {
            pile: Pile::open(&target.path).unwrap(),
            remote,
            requests: Vec::new(),
            unavailable: None,
        };
        let snapshot = store.snapshot().unwrap();

        let opened = runtime()
            .unwrap()
            .block_on(read(&mut store, &snapshot, |reader| {
                open_exact_in(reader, DEFAULT_SCOPE_ID, collection.handle())
            }))
            .unwrap();

        assert_eq!(opened, collection);
        assert_eq!(store.requests, [collection.handle().raw, name.raw]);
        assert!(!snapshot.contains_blob(collection.handle()).unwrap());
        assert!(!snapshot.contains_blob(name).unwrap());
        assert_eq!(store.snapshot().unwrap().wants().unwrap().count(), 0);
        store.pile.close().unwrap();
    }

    #[test]
    fn lazy_list_get_and_show_read_only_their_selected_handles() {
        let mut selected =
            file_capability::stage(b"selected bytes".to_vec(), "chosen.txt", "text/plain").unwrap();
        let selected_id = selected.root().unwrap();
        selected += entity! { ExclusiveId::force_ref(&selected_id) @ file::tag: "chosen" };
        let unrelated =
            file_capability::stage(b"unrelated bytes".to_vec(), "other.png", "image/png").unwrap();
        let (_source, target, facts, mut store) = sparse_store(selected + unrelated);
        let snapshot = store.snapshot().unwrap();
        let content = content_handle_of(&facts, selected_id).unwrap();
        let name = find!(
            handle: TextHandle,
            pattern!(&facts, [{ selected_id @ file::name: ?handle }])
        )
        .next()
        .unwrap();
        let mime = file_capability::media_type_name_handle(&facts, selected_id).unwrap();
        let rt = runtime().unwrap();

        let listed = rt
            .block_on(read(&mut store, &snapshot, |reader| {
                cmd_list(&facts, reader, &["chosen".to_owned()], None)
            }))
            .unwrap();

        assert!(listed.contains("chosen.txt"));
        assert!(!listed.contains("other.png"));
        assert_eq!(
            store.requests.iter().copied().collect::<BTreeSet<_>>(),
            BTreeSet::from([name.raw, mime.raw])
        );
        assert!(!snapshot.contains_blob(name).unwrap());
        assert!(!store.snapshot().unwrap().contains_blob(content).unwrap());

        store.requests.clear();
        let output = target.dir.join("selected.txt");
        prepare_extraction(
            &mut store,
            &rt,
            &facts,
            &snapshot,
            &format!("{selected_id:x}"),
            Some(&output),
        )
        .unwrap()
        .write()
        .unwrap();
        assert_eq!(store.requests, [content.raw]);
        assert_eq!(fs::read(&output).unwrap(), b"selected bytes");
        assert!(!snapshot.contains_blob(content).unwrap());

        let shown = rt
            .block_on(read(&mut store, &snapshot, |reader| {
                cmd_show(&facts, reader, &format!("{selected_id:x}"))
            }))
            .unwrap();
        assert!(shown.contains("Name:     chosen.txt"));
        assert!(shown.contains("MIME:     text/plain"));
        assert_eq!(store.snapshot().unwrap().wants().unwrap().count(), 0);
        store.pile.close().unwrap();
    }

    #[test]
    fn lazy_show_keeps_genuinely_absent_names_and_media_types_optional() {
        let mut fragment = Fragment::empty();
        let content: FileHandle = fragment.put(b"unnamed bytes".to_vec());
        fragment += entity! { metadata::tag: &KIND_FILE, file::content: content };
        let id = fragment.root().unwrap();
        let (_source, _target, facts, mut store) = sparse_store(fragment);
        let snapshot = store.snapshot().unwrap();
        let rt = runtime().unwrap();

        let output = rt
            .block_on(read(&mut store, &snapshot, |reader| {
                cmd_show(&facts, reader, &format!("{id:x}"))
            }))
            .unwrap();

        assert!(output.contains("Name:     ?"));
        assert!(output.contains("MIME:     ?"));
        assert_eq!(read_name(&facts, &snapshot, id).unwrap(), None);
        assert_eq!(read_mime(&facts, &snapshot, id).unwrap(), None);
        assert_eq!(source_path_of(&facts, &snapshot, id).unwrap(), None);
        assert_eq!(store.requests, [content.raw]);
        store.pile.close().unwrap();
    }

    #[test]
    fn lazy_optional_text_distinguishes_missing_bytes_from_undecodable_text() {
        let mut fragment = Fragment::empty();
        let raw: FileHandle = fragment.put(vec![0xff]);
        let malformed: TextHandle = raw.transmute();
        let media_type = entity! {
            metadata::tag: &crate::schemas::files::KIND_MEDIA_TYPE,
            metadata::name: malformed,
        };
        fragment += entity! {
            metadata::tag: &KIND_FILE,
            file::content: raw,
            file::name: malformed,
            file::source_path: malformed,
            file::media_type*: media_type,
        };
        let id = fragment.root().unwrap();
        let (_source, _target, facts, mut store) = sparse_store(fragment);
        let snapshot = store.snapshot().unwrap();

        for error in [
            read_name(&facts, &snapshot, id).unwrap_err(),
            read_mime(&facts, &snapshot, id).unwrap_err(),
            source_path_of(&facts, &snapshot, id).unwrap_err(),
        ] {
            assert_eq!(
                error
                    .chain()
                    .find_map(|error| error.downcast_ref::<MissingBlob>())
                    .unwrap()
                    .handle
                    .raw,
                malformed.raw
            );
        }

        let output = runtime()
            .unwrap()
            .block_on(read(&mut store, &snapshot, |reader| {
                assert_eq!(read_name(&facts, reader, id)?, None);
                assert_eq!(read_mime(&facts, reader, id)?, None);
                assert_eq!(source_path_of(&facts, reader, id)?, None);
                cmd_show(&facts, reader, &format!("{id:x}"))
            }))
            .unwrap();

        assert!(output.contains("Name:     ?"));
        assert!(output.contains("MIME:     ?"));
        assert_eq!(store.requests, [malformed.raw]);
        assert!(!snapshot.contains_blob(malformed).unwrap());
        assert_eq!(store.snapshot().unwrap().wants().unwrap().count(), 0);
        store.pile.close().unwrap();
    }

    #[test]
    fn lazy_directory_get_finishes_reading_before_creating_or_overwriting_files() {
        let first = file_capability::stage(b"first".to_vec(), "first.txt", "text/plain").unwrap();
        let second =
            file_capability::stage(b"second".to_vec(), "second.txt", "text/plain").unwrap();
        let second_content = content_handle_of(second.facts(), second.root().unwrap()).unwrap();
        let mut directory = Fragment::empty();
        let name: TextHandle = directory.put("folder".to_owned());
        directory += entity! {
            metadata::tag: &KIND_DIRECTORY,
            file::name: name,
            file::children*: first + second,
        };
        let id = directory.root().unwrap();
        let (_source, target, facts, mut store) = sparse_store(directory);
        store.unavailable = Some(second_content.raw);
        let snapshot = store.snapshot().unwrap();
        let rt = runtime().unwrap();
        let output = target.dir.join("extracted");

        let error = prepare_extraction(
            &mut store,
            &rt,
            &facts,
            &snapshot,
            &format!("{id:x}"),
            Some(&output),
        )
        .unwrap_err();

        assert_eq!(
            error
                .chain()
                .find_map(|error| error.downcast_ref::<MissingBlob>())
                .unwrap()
                .handle
                .raw,
            second_content.raw
        );
        assert!(
            !output.exists(),
            "failed preparation must not create a partial tree"
        );
        assert_eq!(store.snapshot().unwrap().wants().unwrap().count(), 0);
        store.unavailable = None;
        prepare_extraction(
            &mut store,
            &rt,
            &facts,
            &snapshot,
            &format!("{id:x}"),
            Some(&output),
        )
        .unwrap()
        .write()
        .unwrap();
        assert_eq!(fs::read(output.join("first.txt")).unwrap(), b"first");
        assert_eq!(fs::read(output.join("second.txt")).unwrap(), b"second");
        assert!(!snapshot.contains_blob(second_content).unwrap());
        store.pile.close().unwrap();
    }

    #[cfg(feature = "local-embed")]
    fn native_model_fragment(source: &str, tensor_name: &str, value: f32) -> Fragment {
        use mary::format::attrs;

        let mut fragment = Fragment::empty();
        let data = fragment.put::<mary::format::F32Array, _>(vec![value]);
        let shape = fragment.put::<mary::format::U64Array, _>(vec![1_u64]);
        let leaf = entity! { _ @ attrs::data: data, attrs::shape: shape };
        let leaf_id = leaf.root().unwrap();
        fragment += leaf;

        let tensor_name = fragment.put::<blobencodings::UTF8String, _>(tensor_name.to_owned());
        let member = entity! { _ @
            attrs::kind: "vector",
            attrs::safetensor_path: tensor_name,
            attrs::weight: &leaf_id,
        };
        let member_id = member.root().unwrap();
        fragment += member;

        let root = entity! { _ @ attrs::member: &member_id };
        let root_id = root.root().unwrap();
        fragment += root;
        let source = fragment.put::<blobencodings::UTF8String, _>(source.to_owned());
        fragment += entity! { ExclusiveId::force_ref(&root_id) @
            attrs::source: source,
            attrs::quantization: "native",
        };
        fragment
    }

    #[cfg(feature = "local-embed")]
    fn native_tokenizer_fragment(source: &str, json: &str) -> Fragment {
        let mut fragment = Fragment::empty();
        let tokenizer =
            mary::tokenizer::save_tokenizer_json(json.as_bytes(), source, fragment.blobs_mut())
                .unwrap();
        fragment += tokenizer;
        fragment
    }

    /// The Files similarity query: one `find!` over the WeMM cosines and the
    /// Files facts. Holders come through `file::content`, a content several
    /// entities hold is one key with every holder, the floor is a constraint
    /// and the tag filter is part of the same query. The cosines stand in for
    /// the model's: content blobs that are themselves vectors, under the exact
    /// NVFP4 mapping that keys its rows by the content handle as WeMM does.
    #[cfg(feature = "wemm")]
    #[test]
    fn similar_contents_joins_the_index_to_the_files_facts() {
        use triblespace::core::collection::{AdmissionPolicy, CollectionPolicy};
        use triblespace::core::trible::Trible;
        use triblespace_search::nvfp4::{
            NvFp4CosineIndex, NvFp4CosineSet, NvFp4EmbeddingAttribute,
        };
        use triblespace_search::schemas::Embedding;

        const DIM: usize = 768;
        let vector = |axis: usize, other: Option<(usize, f32)>| {
            let mut v = vec![0.0f32; DIM];
            v[axis] = 1.0;
            if let Some((other, weight)) = other {
                v[other] = weight;
            }
            v
        };
        let key = SigningKey::from_bytes(&[0x74; 32]);
        let root = key.verifying_key();
        let policy =
            CollectionPolicy::new(AdmissionPolicy::direct(root), AdmissionPolicy::direct(root));
        let mut store = MemoryRepo::default();
        let mut put =
            |v: Vec<f32>| -> FileHandle { store.put::<Embedding, _>(v).unwrap().transmute() };
        let exact = put(vector(0, None));
        let close = put(vector(0, Some((1, 0.2))));
        let far = put(vector(5, None));

        let entity = |byte: u8| Id::new([byte; 16]).unwrap();
        let mut facts = TribleSet::new();
        for (holder, content) in [(1, exact), (2, exact), (3, close), (4, far)] {
            facts.insert(&Trible::force(
                &entity(holder),
                &file::content.id(),
                &content,
            ));
        }
        let tagged = entity(1);
        facts += TribleSet::from(entity! { ExclusiveId::force_ref(&tagged) @ file::tag: "form" });

        let source = store.collection("files", policy.clone()).unwrap();
        let target = store
            .derive::<NvFp4CosineSet<Embedding>>(
                source,
                NvFp4EmbeddingAttribute::new(file::content.id(), DIM).unwrap(),
                policy,
            )
            .unwrap();
        store
            .commit(source, &key, Fragment::from(facts.clone()))
            .unwrap();
        let cosines = pollster::block_on(store.maintain(target, &key))
            .unwrap()
            .collection(target)
            .unwrap()
            .view::<NvFp4CosineIndex<Embedding>>()
            .unwrap()
            .reconstructed_cosines(&vector(0, None))
            .unwrap();

        let holders = |floor: f64, tags: &[&str]| {
            let tags: Vec<Inline<inlineencodings::ShortString>> = tags
                .iter()
                .map(|tag| tag.try_to_inline().unwrap())
                .collect();
            similar_contents(&facts, &cosines, floor, &tags)
        };
        let set = |bytes: &[u8]| {
            bytes
                .iter()
                .map(|byte| entity(*byte))
                .collect::<BTreeSet<_>>()
        };

        assert_eq!(
            holders(0.9, &[]),
            BTreeMap::from([(exact, set(&[1, 2])), (close, set(&[3]))])
        );
        assert_eq!(holders(0.99, &[]), BTreeMap::from([(exact, set(&[1, 2]))]));
        assert_eq!(holders(-1.0, &[]).len(), 3, "no floor ranks every content");
        // The tag is asked of the holder, inside the same query.
        assert_eq!(
            holders(0.9, &["form"]),
            BTreeMap::from([(exact, set(&[1]))])
        );
        assert!(holders(0.9, &["form", "other"]).is_empty());
        assert!(cosines.cosine(&exact).unwrap() > 0.999);
    }

    #[cfg(feature = "local-embed")]
    #[test]
    fn native_clip_parts_are_selected_from_one_explicit_snapshot() {
        const CLIP_MODEL: &str = "clip/target";
        let test_pile = TestPile::new();
        let signer = SigningKey::from_bytes(&[0x73; 32]);
        let mut pile = Pile::open(&test_pile.path).unwrap();
        mary::model_collection::publish_model_fragment(
            &mut pile,
            &signer,
            native_model_fragment(CLIP_MODEL, "target.weight", 1.0),
        )
        .unwrap();
        mary::model_collection::publish_model_fragment(
            &mut pile,
            &signer,
            native_model_fragment("clip/distractor", "distractor.weight", 2.0),
        )
        .unwrap();
        mary::model_collection::publish_model_fragment(
            &mut pile,
            &signer,
            native_tokenizer_fragment(CLIP_MODEL, WORDPIECE),
        )
        .unwrap();
        pile.close().unwrap();

        let frozen =
            mary::model_collection::load_model_collection_local_latest(&test_pile.path).unwrap();
        let selected = mary::selection::load_keymap_from_graph(
            frozen.facts(),
            frozen.store(),
            mary::selection::ModelSelector::Source {
                source: CLIP_MODEL,
                quantization: mary::persist::QUANTIZATION_NATIVE,
            },
        )
        .unwrap();
        let tokenizer = mary::selection::load_tokenizer_from_graph(
            frozen.facts(),
            frozen.store(),
            mary::selection::TokenizerSelector::Name(CLIP_MODEL),
        )
        .unwrap();
        assert_eq!(selected["target.weight"], (vec![1.0], vec![1]));
        assert!(!selected.contains_key("distractor.weight"));
        assert_eq!(tokenizer.token_to_id("hello"), Some(1));

        let mut pile = Pile::open(&test_pile.path).unwrap();
        mary::model_collection::publish_model_fragment(
            &mut pile,
            &signer,
            native_model_fragment(CLIP_MODEL, "later.weight", 3.0),
        )
        .unwrap();
        mary::model_collection::publish_model_fragment(
            &mut pile,
            &signer,
            native_tokenizer_fragment(CLIP_MODEL, LATER_WORDPIECE),
        )
        .unwrap();
        pile.close().unwrap();

        let still_selected = mary::selection::load_keymap_from_graph(
            frozen.facts(),
            frozen.store(),
            mary::selection::ModelSelector::Source {
                source: CLIP_MODEL,
                quantization: mary::persist::QUANTIZATION_NATIVE,
            },
        )
        .unwrap();
        let still_tokenizer = mary::selection::load_tokenizer_from_graph(
            frozen.facts(),
            frozen.store(),
            mary::selection::TokenizerSelector::Name(CLIP_MODEL),
        )
        .unwrap();
        assert_eq!(still_selected["target.weight"], (vec![1.0], vec![1]));
        assert!(!still_selected.contains_key("later.weight"));
        assert_eq!(still_tokenizer.token_to_id("hello"), Some(1));
        assert_eq!(still_tokenizer.token_to_id("later"), None);

        let latest =
            mary::model_collection::load_model_collection_local_latest(&test_pile.path).unwrap();
        let latest_selected = mary::selection::load_keymap_from_graph(
            latest.facts(),
            latest.store(),
            mary::selection::ModelSelector::Source {
                source: CLIP_MODEL,
                quantization: mary::persist::QUANTIZATION_NATIVE,
            },
        )
        .unwrap();
        assert_eq!(latest_selected["target.weight"], (vec![1.0], vec![1]));
        assert_eq!(latest_selected["later.weight"], (vec![3.0], vec![1]));
        let tokenizer_error = mary::selection::load_tokenizer_from_graph(
            latest.facts(),
            latest.store(),
            mary::selection::TokenizerSelector::Name(CLIP_MODEL),
        )
        .unwrap_err();
        assert!(
            tokenizer_error.to_string().contains("ambiguous"),
            "{tokenizer_error}"
        );
    }

    #[test]
    fn empty_native_collection_opens_as_an_empty_catalog() {
        let test_pile = TestPile::new();
        let storage = Storage::new(test_pile.path.clone(), None);
        with_files_view(&storage, |_, _, _, space, _reader, _rt| {
            assert!(find!(
                id: Id,
                pattern!(space, [{ ?id @ metadata::tag: _?kind }])
            )
            .next()
            .is_none());
            cmd_list(space, _reader, &[], None)
        })
        .unwrap();
    }

    #[test]
    fn every_reader_sees_every_files_commit_attached_or_not() {
        let test_pile = TestPile::new();
        let owner = Storage::new(test_pile.path.clone(), None);
        let first = file_capability::stage(b"first".to_vec(), "first.txt", "text/plain").unwrap();
        let second =
            file_capability::stage(b"second".to_vec(), "second.txt", "text/plain").unwrap();
        let first_id = first.root().unwrap();
        let second_id = second.root().unwrap();

        fn file_ids(space: &FactArchive) -> BTreeSet<Id> {
            find!(
                entity: Id,
                pattern!(space, [{ ?entity @ metadata::tag: &KIND_FILE }])
            )
            .collect()
        }

        // The first fixture is a raw commit the worker has carried into the
        // views; the second is a raw commit nobody has carried yet.
        with_files_store(&owner, |store, collection, signer, _| {
            store
                .commit(collection, signer, first)
                .context("commit first fixture")?;
            // A raw commit is the worker's to carry, never a reader's.
            crate::storage::carry_facts(store, collection, signer);
            store
                .commit(collection, signer, second)
                .context("commit second fixture")?;
            Ok(())
        })
        .unwrap();
        // A read maintains nothing: it takes the attachment of the carried
        // commit and reads the uncarried one from its own bytes.
        with_files_view(&owner, |_, _, _, space, _, _| {
            assert_eq!(file_ids(space), BTreeSet::from([first_id, second_id]));
            Ok(())
        })
        .unwrap();

        // A second key reads the owner's collection: it must neither fail
        // for want of rights nor publish anything, and it sees what the owner
        // saw, from the bytes, since it believes none of the owner's MAPs.
        let reader_key = test_pile.dir.join("reader.key");
        initialize_signer(&test_pile.path, Some(&reader_key)).unwrap();
        let reader = Storage::new(test_pile.path.clone(), Some(reader_key));
        let authority = load_signer(&test_pile.path, None).unwrap().verifying_key();
        reader
            .with_store(|store, signer, runtime| {
                let collection = open(store, DEFAULT_SCOPE_ID, authority)
                    .context("open the owner's Files collection")?;
                files_view_in(
                    store,
                    collection,
                    signer,
                    runtime,
                    |_, _, _, space, _, _| {
                        assert_eq!(file_ids(space), BTreeSet::from([first_id, second_id]));
                        Ok(())
                    },
                )
            })
            .unwrap();

        // Once the worker carries the second commit, both readers still see
        // both.
        with_files_store(&owner, |store, collection, signer, _| {
            crate::storage::carry_facts(store, collection, signer);
            Ok(())
        })
        .unwrap();
        with_files_view(&owner, |_, _, _, space, _, _| {
            assert_eq!(file_ids(space), BTreeSet::from([first_id, second_id]));
            Ok(())
        })
        .unwrap();
        reader
            .with_store(|store, signer, runtime| {
                let collection = open(store, DEFAULT_SCOPE_ID, authority)
                    .context("open the owner's Files collection")?;
                files_view_in(
                    store,
                    collection,
                    signer,
                    runtime,
                    |_, _, _, space, _, _| {
                        assert_eq!(file_ids(space), BTreeSet::from([first_id, second_id]));
                        Ok(())
                    },
                )
            })
            .unwrap();
    }

    #[test]
    fn independent_commits_materialize_for_list_show_and_get() {
        let test_pile = TestPile::new();
        let storage = Storage::new(test_pile.path.clone(), None);
        let first =
            file_capability::stage(b"first file".to_vec(), "first.png", "image/png").unwrap();
        let second =
            file_capability::stage(b"second file".to_vec(), "second.txt", "text/plain").unwrap();
        let first_id = first.root().unwrap();
        let second_id = second.root().unwrap();

        with_files_store(&storage, |store, collection, signer, _| {
            store
                .commit(collection, signer, first)
                .context("commit first fixture")?;
            store
                .commit(collection, signer, second)
                .context("commit second fixture")?;
            // A raw commit is the worker's to carry.
            crate::storage::carry_facts(store, collection, signer);
            Ok(())
        })
        .unwrap();

        let first_out = test_pile.dir.join("first.png");
        let second_out = test_pile.dir.join("second.txt");
        with_files_view(&storage, |store, _, _, space, reader, rt| {
            assert_eq!(
                find!(
                    entity: Id,
                    pattern!(space, [{ ?entity @ metadata::tag: &KIND_FILE }])
                )
                .collect::<BTreeSet<_>>()
                .len(),
                2
            );
            cmd_list(space, reader, &[], None)?;
            cmd_show(space, reader, &format!("{first_id:x}"))?;
            cmd_show(space, reader, &format!("{second_id:x}"))?;
            prepare_extraction(
                store,
                rt,
                space,
                reader,
                &format!("{first_id:x}"),
                Some(&first_out),
            )?
            .write()?;
            prepare_extraction(
                store,
                rt,
                space,
                reader,
                &format!("{second_id:x}"),
                Some(&second_out),
            )
        })
        .unwrap()
        .write()
        .unwrap();
        assert_eq!(fs::read(first_out).unwrap(), b"first file");
        assert_eq!(fs::read(second_out).unwrap(), b"second file");
    }

    #[test]
    fn replaying_one_complete_fragment_is_idempotent() {
        let test_pile = TestPile::new();
        let storage = Storage::new(test_pile.path.clone(), None);
        let file = file_capability::stage(b"same".to_vec(), "same.txt", "text/plain").unwrap();
        let file_id = file.root().unwrap();

        with_files_store(&storage, |store, collection, signer, _| {
            let first = store
                .commit(collection, signer, file.clone())
                .context("first replay")?;
            // A raw commit is the worker's to carry; carrying after each
            // replay is what makes the view's count below an idempotence claim.
            crate::storage::carry_facts(store, collection, signer);
            let second = store
                .commit(collection, signer, file)
                .context("second replay")?;
            assert_eq!(first, second);
            crate::storage::carry_facts(store, collection, signer);
            Ok(())
        })
        .unwrap();

        with_files_view(&storage, |_, _, _, space, _reader, _rt| {
            assert_eq!(
                file_capability::resolve_selector(space, &format!("{file_id:x}"))?,
                file_id
            );
            assert_eq!(
                find!(
                    entity: Id,
                    pattern!(space, [{ ?entity @ metadata::tag: &KIND_FILE }])
                )
                .collect::<BTreeSet<_>>()
                .len(),
                1
            );
            Ok(())
        })
        .unwrap();
    }
}
