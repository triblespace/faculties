//! Files operations over maintained, immutable collection views.
//!
//! Callers use typed operations directly; no CLI invocation or MCP value enters here.

#[cfg(feature = "wemm")]
#[path = "wemm.rs"]
pub mod wemm;

use crate::clock;
use crate::collection_names::{read_union_acquiring, write_target_acquiring};
#[cfg(test)]
use crate::collection_names::open;
#[cfg(test)]
use crate::collection_names::open_exact_in;
use crate::files as file_capability;
use crate::out::Out;
use crate::schemas::embeddings;
use crate::schemas::files::{
    file, page, DEFAULT_SCOPE_ID, KIND_DIRECTORY, KIND_FILE, KIND_IMPORT, KIND_PAGE,
};
#[cfg(test)]
use crate::storage::FactRead;
use crate::storage::{read, AcquiringReader, FactArchive, FacultySnapshot, FacultyStore, Storage};
use anyhow::{bail, Context, Result};
use ed25519_dalek::SigningKey;
use hifitime::efmt::consts::ISO8601_DATE;
use hifitime::efmt::Formatter;
use hifitime::Epoch;
#[cfg(feature = "local-embed")]
use mary::embed::LocalEmbedder as _;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use triblespace::core::blob::encodings::simplearchive::SimpleArchive;
#[cfg(test)]
use triblespace::core::blob::encodings::succinctarchive::{
    Rank9AcceleratedSuccinctArchiveBlob, SuccinctArchiveBlob,
};
use triblespace::core::collection::{Collection, CollectionStoreExt};
#[cfg(feature = "local-embed")]
use triblespace::core::collection::CollectionSnapshotExt;
use triblespace::core::metadata;
use triblespace::core::query::TriblePattern;
use triblespace::core::repo::async_store::AsyncBlobStoreAcquire;
#[cfg(test)]
use triblespace::core::repo::pile::Pile;
use triblespace::core::repo::pile::PileSnapshot;
use triblespace::core::repo::{BlobStoreGet, BlobStoreList, SnapshotSource};
use triblespace::prelude::*;
#[cfg(feature = "local-embed")]
use triblespace_search::nvfp4::{NvFp4CosineIndex, NvFp4CosineSet, ReconstructedCosines};
#[cfg(feature = "local-embed")]
use triblespace_search::semantic::{
    classify, local_compute, Content, SemanticIndex, SemanticModel,
};

// ── type aliases ─────────────────────────────────────────────────────────
type FileHandle = Inline<inlineencodings::Handle<blobencodings::RawBytes>>;
type TextHandle = Inline<inlineencodings::Handle<blobencodings::UTF8String>>;
/// Handle into the nomic-embed-multimodal-7b dense space (3584-d), distinct
/// from the shared 768-d nomic space the semantic index lives in.
type Mm7bHandle = Inline<inlineencodings::Handle<embeddings::Embedding3584>>;

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

/// If `eid` is a rasterized-PDF page entity, return its `(parent file id, page
/// index label)`. Used by the 7b similarity display so a page hit reads back as
/// "file X, page N" instead of a nameless entity.
fn read_page<P: TriblePattern>(space: &P, eid: Id) -> Option<(Id, String)> {
    find!(
        (parent: Id, idx: String),
        pattern!(space, [{ eid @ metadata::tag: &KIND_PAGE, page::parent: ?parent, page::index: ?idx }])
    )
    .next()
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
    let target = storage.target();
    storage.with_store(|store, signer, runtime| {
        let collection = write_target_acquiring(
            store,
            DEFAULT_SCOPE_ID,
            signer.verifying_key(),
            target,
            runtime,
        )?;
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
        let sources =
            read_union_acquiring(store, DEFAULT_SCOPE_ID, signer.verifying_key(), runtime)?;
        files_view_in(store, &sources, runtime, |store, facts, snapshot, runtime| {
            f(store, collection, signer, facts, snapshot, runtime)
        })
    })
}

/// [`with_files_view`] for a command that only reads: it chooses no write
/// target, so a pile whose default target is ambiguous still reads.
fn with_files_read<T>(
    storage: &Storage,
    f: impl FnOnce(
        &mut FacultyStore,
        &FactArchive,
        &FacultySnapshot,
        &std::sync::Arc<tokio::runtime::Runtime>,
    ) -> Result<T>,
) -> Result<T> {
    storage.with_store(|store, signer, runtime| {
        let sources =
            read_union_acquiring(store, DEFAULT_SCOPE_ID, signer.verifying_key(), runtime)?;
        files_view_in(store, &sources, runtime, f)
    })
}

/// Attach the Files views of a read-union's collections as one fact
/// archive; see [`with_files_view`] for the maintenance rule.
fn files_view_in<T>(
    store: &mut FacultyStore,
    sources: &[Collection<SimpleArchive>],
    runtime: &std::sync::Arc<tokio::runtime::Runtime>,
    f: impl FnOnce(
        &mut FacultyStore,
        &FactArchive,
        &FacultySnapshot,
        &std::sync::Arc<tokio::runtime::Runtime>,
    ) -> Result<T>,
) -> Result<T> {
    let rank9 =
        crate::storage::rank9_union(store, sources).context("register the Files fact collections")?;
    let reader = store
        .snapshot()
        .context("freeze the Files views as they stand")?;
    let acquiring = AcquiringReader::new(reader.clone(), runtime.clone());
    let space = crate::storage::acquire_union_facts(&acquiring, &rank9)
        .context("read Files fact collection")?;
    f(store, &space, &reader, runtime)
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

// ── embedder seam (mary, behind `local-embed`) ────────────────────────────
/// The compute class the Files semantic index is canonical on: the Sparks.
/// The descriptor is the same from every machine, so one index exists, and a
/// machine of another class reads the rows that replicate to it and derives
/// none: the mapping is pinned to this class. A machine of this class embeds
/// every file whose bytes it holds that has no usable row, whoever saved
/// it, with any key the index admits; a file whose bytes no such machine
/// holds has no row yet. `files index` and `files similar` count those files
/// instead of promising that replication will bring them.
#[cfg(feature = "local-embed")]
const SEMANTIC_COMPUTE: &str = "gb10";

/// The semantic index of one kind over this Files collection, as a function
/// of the working pile's model roots: every `file::content` value whose bytes
/// that kind's model reads (images through the pinned nomic-vision root, PDF
/// text layers and UTF-8 through the nomic-text root), one row per distinct
/// content handle. The same descriptor from every machine, so `files similar`
/// never has to discover it. Only changing the selected references creates a
/// new descriptor; another model or observation in the same collection does
/// not.
#[cfg(feature = "local-embed")]
fn semantic_index<R: triblespace::core::repo::StoreRead>(
    descriptors: &R,
    kind: Kind,
    compute: &str,
) -> Result<SemanticIndex<embeddings::Embedding768>> {
    let models = crate::nomic::index_models_in(descriptors)?;
    let model = match kind {
        Kind::Image => SemanticModel::Vision {
            root: models.vision_root,
        },
        Kind::Text => SemanticModel::Text {
            root: models.text_root,
            tokenizer: models.tokenizer_root,
        },
    };
    SemanticIndex::new(
        [file::content.id()],
        models.collection,
        model,
        compute,
        embeddings::DIM,
    )
    .map_err(|error| anyhow::anyhow!("describe the Files {kind} index: {error}"))
}

/// What a Files commit without semantic rows means, for a reader: rows come
/// only from the key that wrote a file, on the canonical compute.
#[cfg(feature = "local-embed")]
fn semantic_lag_note(kind: Kind, unindexed: usize) -> String {
    format!(
        "note: {unindexed} Files commit(s) have no {kind} rows here yet. `files index` on a \
         {SEMANTIC_COMPUTE} embeds every file whose bytes it holds, whoever saved it; \
         elsewhere the rows arrive by replication"
    )
}

/// Register the index descriptor of one kind (idempotent) from the same
/// frozen model observation used by its query or golden check.
#[cfg(feature = "local-embed")]
fn semantic_target<R: triblespace::core::repo::StoreRead>(
    store: &mut FacultyStore,
    collection: Collection<SimpleArchive>,
    kind: Kind,
    compute: &str,
    descriptors: &R,
) -> Result<Collection<NvFp4CosineSet<embeddings::Embedding768>>> {
    let policy = collection
        .policy(descriptors)
        .context("read Files source collection policy")?;
    let index = semantic_index(descriptors, kind, compute)?;
    store
        .derive_with(collection, index, policy)
        .with_context(|| format!("register the Files {kind} index"))
}

/// Both semantic indexes as one pass left them: each index maintained, the
/// snapshot that sees the result, and what failed, one error per index that
/// did.
#[cfg(feature = "local-embed")]
type SemanticUpkeep = (
    Vec<(Kind, Collection<NvFp4CosineSet<embeddings::Embedding768>>)>,
    FacultySnapshot,
    Vec<anyhow::Error>,
);

/// Maintain both indexes and return them, the snapshot that sees the
/// result, and what failed.
///
/// With `every_file` false -- what saving a file does -- embed the Files
/// commits this key wrote that have no row at all yet: the file just saved,
/// and any earlier one of this key's still without one. A commit whose row's
/// bytes are not here is left to `files index`. With it true -- `files
/// index` -- embed every Files commit whose bytes are here and that has no
/// row whose bytes are here or can be fetched, whoever wrote it, then carry
/// each index's rows into this key's merges. Only a machine of the canonical
/// compute embeds, and only after its golden vectors agree; on any other the
/// mapping is pinned elsewhere, nothing is embedded, and `every_file` still
/// carries the rows that arrived by replication.
///
/// One index failing -- a file its model refuses, rows its carry cannot
/// join -- holds back neither the other index nor what the caller reports:
/// each is maintained as far as it goes, and the failures come back with
/// both. Only a failed golden check, or a pile that cannot be read at all,
/// ends the pass as an error; a pile that cannot be read once the indexes
/// were maintained ends it with what they reported as well.
#[cfg(feature = "local-embed")]
fn maintain_semantic(
    store: &mut FacultyStore,
    collection: Collection<SimpleArchive>,
    signer: &SigningKey,
    runtime: &std::sync::Arc<tokio::runtime::Runtime>,
    every_file: bool,
) -> Result<SemanticUpkeep> {
    maintain_semantic_on(
        store,
        collection,
        signer,
        runtime,
        every_file,
        SEMANTIC_COMPUTE,
    )
}

/// [`maintain_semantic`] for indexes computed on the class `compute`: the
/// one place that decides what this machine does with them.
#[cfg(feature = "local-embed")]
fn maintain_semantic_on(
    store: &mut FacultyStore,
    collection: Collection<SimpleArchive>,
    signer: &SigningKey,
    runtime: &std::sync::Arc<tokio::runtime::Runtime>,
    every_file: bool,
    compute: &str,
) -> Result<SemanticUpkeep> {
    // Golden checks and both target descriptors use the same observation.
    // Acquiring a body may add bytes, never change the model chosen midway.
    let frozen = AcquiringReader::new(
        store.snapshot().context("freeze the Files semantic models")?,
        runtime.clone(),
    );
    if local_compute() == compute {
        // The golden vectors first: this device must embed the fixed inputs
        // to what the model collection records before it publishes a row.
        crate::nomic::golden_report(&frozen)?.admit()?;
    }
    let mut targets = Vec::with_capacity(Kind::ALL.len());
    let mut failures = Vec::new();
    for kind in Kind::ALL {
        let target = match semantic_target(store, collection, kind, compute, &frozen) {
            Ok(target) => target,
            Err(error) => {
                failures.push(error);
                continue;
            }
        };
        let maintained = runtime
            .block_on(async {
                if every_file {
                    store
                        .maintain_with::<SemanticIndex<embeddings::Embedding768>>(target, signer)
                        .await
                } else {
                    store
                        .ensure_with::<SemanticIndex<embeddings::Embedding768>>(target, signer)
                        .await
                }
            })
            .with_context(|| format!("maintain the Files {kind} index"));
        if let Err(error) = maintained {
            failures.push(error);
        }
        targets.push((kind, target));
    }
    let snapshot = match store
        .snapshot()
        .context("freeze the pile after the Files semantic indexes")
    {
        Ok(snapshot) => snapshot,
        Err(error) => {
            failures.push(error);
            return Err(semantic_failures(failures).expect_err("a failure was just recorded"));
        }
    };
    Ok((targets, snapshot, failures))
}

/// One error naming every index that failed, or none.
#[cfg(feature = "local-embed")]
fn semantic_failures(failures: Vec<anyhow::Error>) -> Result<()> {
    if failures.is_empty() {
        return Ok(());
    }
    bail!(
        "{}",
        failures
            .iter()
            .map(|error| format!("{error:#}"))
            .collect::<Vec<_>>()
            .join("; ")
    )
}

/// `files golden`: how this device embeds the golden inputs against the
/// vectors the model collection records; `--publish` records separate
/// observations referring to roots that have none, from the canonical compute.
#[cfg(feature = "local-embed")]
fn cmd_golden(
    store: &mut FacultyStore,
    signer: &SigningKey,
    runtime: &std::sync::Arc<tokio::runtime::Runtime>,
    publish: bool,
    out: &mut Out<'_>,
) -> Result<()> {
    let (report, recorded) = if publish {
        if local_compute() != SEMANTIC_COMPUTE {
            bail!(
                "golden vectors are recorded on {SEMANTIC_COMPUTE} and this machine is {}",
                local_compute()
            );
        }
        crate::nomic::golden_publish(store, signer, runtime)?
    } else {
        let frozen = AcquiringReader::new(
            store.snapshot().context("freeze the pile for the golden vectors")?,
            runtime.clone(),
        );
        (crate::nomic::golden_report(&frozen)?, Vec::new())
    };
    for row in &report.rows {
        let state = if recorded.contains(&row.model) {
            "recorded now from this device".to_string()
        } else {
            match row.cosine() {
                Some(cos) => format!("cos {cos:.5} to the recorded vector"),
                None => "no vector recorded".to_string(),
            }
        };
        out.line(format!("{:<5} root {:X}  {state}", row.model, row.root))?;
    }
    out.line(format!(
        "computed on {}; a device below cos {} does not publish rows",
        local_compute(),
        crate::nomic::golden::FLOOR
    ))
}

#[cfg(feature = "local-embed")]
fn collection_hex(handle: triblespace::core::collection::CollectionHandle) -> String {
    hex::encode(handle.raw)
}

// ── nomic-embed-multimodal-7b seam (3584-d dense space) ───────────────────
// A SEPARATE, additive path from the shared 768-d one above. The 7b model embeds both
// images (`embed_image`, pure-Rust decode→preprocess→vision→backbone) and text
// queries (`embed_query`) into one 3584-d space — strong text→image retrieval.
// Loaded once per command (cold mmap ~20s, then ~0.5-1s/embed). macOS/Metal
// only; gated behind `local-embed`.

#[cfg(all(feature = "local-embed", target_os = "macos"))]
type Mm7bEmbedder =
    mary::models::qwen2_5_vl::embedder::NomicMultimodalEmbedder<mary::nn::backend::B>;

/// Default weights pile + tokenizer for the 7b. Both can be overridden, while
/// the tokenizer's ordinary fallback is resolved from the Hugging Face cache.
#[cfg(all(feature = "local-embed", target_os = "macos"))]
fn load_mm7b() -> Result<Mm7bEmbedder> {
    const MODEL: &str = "nomic-ai/nomic-embed-multimodal-7b";
    let pile = match std::env::var_os("NOMIC_MM7B_PILE") {
        Some(p) => PathBuf::from(p),
        None => crate::model_dir().join("nomic_mm7b.pile"),
    };
    let tok = match std::env::var_os("NOMIC_MM7B_TOKENIZER") {
        Some(path) => PathBuf::from(path),
        None => {
            let path = mary::embed::hf_cache_main_snapshot(MODEL)?.join("tokenizer.json");
            anyhow::ensure!(
                path.is_file(),
                "tokenizer.json not in cached main revision for {MODEL}; set NOMIC_MM7B_TOKENIZER"
            );
            path
        }
    };
    eprintln!("files: loading nomic-embed-multimodal-7b (once, ~20s)…");
    crate::model_storage::with_snapshot(&pile, MODEL, |snapshot| {
        mary::persist::load_nomic_mm7b_aliased_from_snapshot(
            snapshot.clone(),
            &tok,
            mary::nn::backend::WgpuDevice::default(),
        )
    })
}

/// Embed image bytes into the 3584-d 7b space.
#[allow(unused_variables)]
fn mm7b_embed_image(emb: &Mm7bEmbedderOpt, bytes: &[u8]) -> Result<Vec<f32>> {
    #[cfg(all(feature = "local-embed", target_os = "macos"))]
    {
        return emb.embed_image(bytes);
    }
    #[cfg(not(all(feature = "local-embed", target_os = "macos")))]
    bail!("`files embed-7b` needs the 7b embedder — rebuild with --features local-embed on macOS");
}

/// Embed a text query into the 3584-d 7b space (query-side augmentation).
#[allow(unused_variables)]
fn mm7b_embed_query(emb: &Mm7bEmbedderOpt, text: &str) -> Result<Vec<f32>> {
    #[cfg(all(feature = "local-embed", target_os = "macos"))]
    {
        return emb.embed_query(text);
    }
    #[cfg(not(all(feature = "local-embed", target_os = "macos")))]
    bail!(
        "`files similar --mm7b --text` needs the 7b embedder — rebuild with --features local-embed on macOS"
    );
}

// A tiny alias so the helper signatures above are the same with/without the
// feature: with it, the concrete embedder; without it, the unit type (the
// helpers `bail!` before ever touching the value).
#[cfg(all(feature = "local-embed", target_os = "macos"))]
type Mm7bEmbedderOpt = Mm7bEmbedder;
#[cfg(not(all(feature = "local-embed", target_os = "macos")))]
type Mm7bEmbedderOpt = ();

/// Construct the 7b embedder, or `bail!` cleanly when the feature/platform is
/// absent. Returns the concrete embedder (feature) or `()` (no feature, after a
/// bail — so the call site never proceeds without a real model).
#[allow(unreachable_code)]
fn load_mm7b_opt() -> Result<Mm7bEmbedderOpt> {
    #[cfg(all(feature = "local-embed", target_os = "macos"))]
    {
        return load_mm7b();
    }
    #[cfg(not(all(feature = "local-embed", target_os = "macos")))]
    bail!(
        "the nomic-embed-multimodal-7b path needs `--features local-embed` on macOS (Metal); \
         this build doesn't have it"
    );
}

/// Read a stored 3584-d embedding blob back into a plain `Vec<f32>`.
fn read_embedding_3584<R: BlobStoreGet>(reader: &R, h: Mm7bHandle) -> Result<Vec<f32>> {
    let v: anybytes::View<[f32]> = reader.get(h).context("read 7b embedding blob")?;
    Ok(v.as_ref().to_vec())
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
    ensure_files_after_commit(pile, collection, signer, runtime)?;

    // A saved image is searchable the moment it is saved, where this machine
    // can embed it (JP, 2026-09-12: saving an image should embed it, no skip
    // path); elsewhere its rows arrive from a machine that can. Saving embeds
    // every file of this key's that has no row at all, not only this one.
    #[cfg(feature = "local-embed")]
    if local_compute() == SEMANTIC_COMPUTE {
        match maintain_semantic(pile, collection, signer, runtime, false) {
            Ok((_, _, failures)) => {
                for error in failures {
                    out.line(format!("Semantic index not maintained: {error:#}"))?;
                }
            }
            Err(error) => out.line(format!("Semantic index not maintained: {error:#}"))?,
        }
    }
    #[cfg(not(feature = "local-embed"))]
    let _ = runtime;

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

/// Maintain the Files semantic indexes: every distinct value under
/// `file::content` embedded through the model of its kind in the working pile
/// (images through nomic-vision, PDF text layers and UTF-8 through
/// nomic-text), as rows of two derived NVFP4 cosine sets keyed by content
/// handle (see `triblespace_search::semantic`). Idempotent: a file with a
/// row whose bytes are here or can be fetched is not embedded again, whoever
/// derived that row, and only missing DERIVE work is computed. A machine of
/// the canonical compute class embeds every file whose bytes it holds,
/// whoever saved it; any other machine embeds nothing and reads the rows
/// that replicate to it. Either carries each index's rows into its own
/// merges, and the report counts the Files commits that have no row yet
/// (`SEMANTIC_COMPUTE` says why). One index failing holds back neither the
/// other nor either report; every failure is returned after both.
fn cmd_index(
    store: &mut FacultyStore,
    collection: Collection<SimpleArchive>,
    signer: &SigningKey,
    runtime: &std::sync::Arc<tokio::runtime::Runtime>,
    out: &mut Out<'_>,
) -> Result<()> {
    #[cfg(not(feature = "local-embed"))]
    {
        let _ = (store, collection, signer, runtime, out);
        bail!("`files index` needs the embedders — rebuild with --features local-embed");
    }
    #[cfg(feature = "local-embed")]
    index_on(store, collection, signer, runtime, out, SEMANTIC_COMPUTE)
}

/// [`cmd_index`] for indexes computed on the class `compute`.
#[cfg(feature = "local-embed")]
fn index_on(
    store: &mut FacultyStore,
    collection: Collection<SimpleArchive>,
    signer: &SigningKey,
    runtime: &std::sync::Arc<tokio::runtime::Runtime>,
    out: &mut Out<'_>,
    compute: &str,
) -> Result<()> {
    let (targets, snapshot, mut failures) =
        maintain_semantic_on(store, collection, signer, runtime, true, compute)?;
    let snapshot = AcquiringReader::new(snapshot, runtime.clone());
    for (kind, target) in targets {
        let index = snapshot
            .collection_acquiring(target)
            .with_context(|| format!("observe the Files {kind} index"))
            .and_then(|observed| {
                observed
                    .view::<NvFp4CosineIndex<embeddings::Embedding768>>()
                    .with_context(|| format!("read the Files {kind} index"))
            });
        match index {
            Ok(index) => out.line(format!(
                "Files {kind} index {}: {} member(s), {} row(s), computed on {compute}",
                collection_hex(target.handle()),
                index.segment_count(),
                index.len(),
            ))?,
            Err(error) => failures.push(error),
        }
        match crate::storage::underived(&snapshot, collection, target) {
            Ok(0) => {}
            Ok(unindexed) => out.line(semantic_lag_note(kind, unindexed))?,
            Err(error) => {
                failures.push(error.context(format!("count Files commits without {kind} rows")))
            }
        }
    }
    semantic_failures(failures)
}

/// Embed every image file with nomic-embed-multimodal-7b and store the 3584-d
/// vector on `attr_mm7b::embedding` (under the file's intrinsic record id, so
/// identity is unaffected — pure exhaust). Additive to the shared-space `attr::embedding`
/// path: both coexist. Idempotent — already-embedded files are skipped unless
/// `--force`. Identical bytes (duplicate imports) are embedded once and the
/// vector fanned out to every entity that shares the content.
fn cmd_embed7b<P: TriblePattern>(
    pile: &mut FacultyStore,
    collection: Collection<SimpleArchive>,
    signer: &SigningKey,
    runtime: &tokio::runtime::Runtime,
    space: &P,
    reader: &impl BlobStoreGet,
    force: bool,
    out: &mut Out<'_>,
) -> Result<()> {
    // Gather image file entities, grouped by content hash so identical bytes are
    // embedded once. Skip SVG (not a raster the vision tower can decode).
    let mut groups: BTreeMap<String, (FileHandle, Vec<(Id, bool)>)> = BTreeMap::new();
    for (eid, h) in find!(
        (eid: Id, h: FileHandle),
        pattern!(space, [{ ?eid @ metadata::tag: &KIND_FILE, file::content: ?h }])
    ) {
        let mime = read_mime(space, reader, eid)?.unwrap_or_default();
        if !mime.starts_with("image/") || mime == "image/svg+xml" {
            continue;
        }
        let has_emb = exists!(
            (e: Mm7bHandle),
            pattern!(space, [{ eid @ embeddings::attr_mm7b::embedding: ?e }])
        );
        groups
            .entry(handle_hex(h))
            .or_insert_with(|| (h, Vec::new()))
            .1
            .push((eid, has_emb));
    }

    if groups.is_empty() {
        out.line(format!("(no image files to embed)"))?;
        return Ok(());
    }

    // Which groups still need work?
    let pending: Vec<_> = groups
        .into_iter()
        .filter(|(_, (_, eids))| force || eids.iter().any(|(_, has)| !*has))
        .collect();

    let total_imgs: usize = pending.iter().map(|(_, (_, e))| e.len()).sum();
    if pending.is_empty() {
        out.line(format!(
            "All image files already have a 7b embedding (use --force to re-embed)."
        ))?;
        return Ok(());
    }

    let embedder = load_mm7b_opt()?;

    let mut change = Fragment::empty();
    let mut embedded = 0usize;
    let mut assigned = 0usize;
    let mut failed = 0usize;
    for (hash, (content, eids)) in &pending {
        let bytes: anybytes::Bytes = match reader.get::<anybytes::Bytes, _>(*content) {
            Ok(b) => b,
            Err(e) => {
                out.line(format!("  skip {hash}: read content failed: {e:?}"))?;
                failed += 1;
                continue;
            }
        };
        let v = match mm7b_embed_image(&embedder, bytes.as_ref()) {
            Ok(v) => v,
            Err(e) => {
                out.line(format!("  skip {hash}: embed failed: {e:#}"))?;
                failed += 1;
                continue;
            }
        };
        embedded += 1;
        for (eid, has) in eids {
            if *has && !force {
                continue;
            }
            let handle: Mm7bHandle = change.put::<embeddings::Embedding3584, _>(v.clone());
            change += entity! {
                ExclusiveId::force_ref(eid) @ embeddings::attr_mm7b::embedding: handle
            };
            assigned += 1;
        }
        out.line(format!(
            "  embedded {hash}  ({} bytes → 3584-d)",
            bytes.len()
        ))?;
    }

    if change.is_empty() {
        out.line(format!(
            "Nothing to commit (embedded {embedded}, failed {failed})."
        ))?;
        return Ok(());
    }

    pile.commit(collection, signer, change)
        .context("commit Files 7b embeddings")?;
    ensure_files_after_commit(pile, collection, signer, runtime)?;

    out.line(format!(
        "7b-embedded {embedded} unique images → {assigned} file entities (of {total_imgs} pending){}",
        if failed > 0 {
            format!(", {failed} failed")
        } else {
            String::new()
        },
    ))?;
    Ok(())
}

// ── PDF rasterization (page-level 7b embedding) ────────────────────────────
// A PDF isn't an image — to put it in the nomic-mm7b space we rasterize each
// page to a PNG and embed that. We shell out to `pdftoppm` (poppler): it is
// already present on this machine, renders robustly to RGB, has no heavy
// build-time C dependency (unlike `mupdf`) and no runtime dylib to vendor
// (unlike `pdfium-render`, which needs `libpdfium`). The only cost is a runtime
// dependency on `pdftoppm` being on PATH — checked up front with a clear bail.
// nomic-embed-multimodal-7b is itself a *visual document* retrieval model
// (ColPali-style, trained on page screenshots), so a rendered page is exactly
// its native input — this is the path the model is strongest at.

/// Render a PDF (raw bytes) to per-page PNGs via `pdftoppm`. Returns
/// `(page_number, png_bytes)` sorted by page, 1-based. `max_pages == 0` renders
/// all pages; otherwise only the first `max_pages`. Pure side-effect-free from
/// the pile's view: writes to a private temp dir that is removed on return.
fn render_pdf_pages(bytes: &[u8], dpi: u32, max_pages: usize) -> Result<Vec<(usize, Vec<u8>)>> {
    use std::process::Command as PCommand;

    if which_pdftoppm().is_none() {
        bail!(
            "`pdftoppm` not found on PATH — install poppler (e.g. `brew install poppler`) \
             to rasterize PDFs for 7b embedding"
        );
    }

    // Private temp dir under the system temp root; cleaned up before returning.
    let dir = std::env::temp_dir().join(format!("files_pdf7b_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).with_context(|| format!("create temp dir {dir:?}"))?;
    let in_pdf = dir.join("in.pdf");
    fs::write(&in_pdf, bytes).with_context(|| format!("write temp pdf {in_pdf:?}"))?;
    let prefix = dir.join("page");

    let mut cmd = PCommand::new("pdftoppm");
    cmd.arg("-png").arg("-r").arg(dpi.to_string());
    if max_pages > 0 {
        cmd.arg("-l").arg(max_pages.to_string());
    }
    cmd.arg(&in_pdf).arg(&prefix);
    let out = cmd.output().with_context(|| "spawn pdftoppm")?;
    if !out.status.success() {
        let _ = fs::remove_dir_all(&dir);
        bail!(
            "pdftoppm failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    // Collect page PNGs: pdftoppm names them `<prefix>-<n>.png`, n zero-padded
    // to the page-count width. Parse the trailing number so order is numeric.
    let mut pages: Vec<(usize, Vec<u8>)> = Vec::new();
    for entry in fs::read_dir(&dir).with_context(|| format!("read temp dir {dir:?}"))? {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("png") {
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        let num = stem
            .rsplit('-')
            .next()
            .and_then(|d| d.parse::<usize>().ok());
        if let Some(n) = num {
            let data = fs::read(&path).with_context(|| format!("read page {path:?}"))?;
            pages.push((n, data));
        }
    }
    let _ = fs::remove_dir_all(&dir);
    pages.sort_by_key(|(n, _)| *n);
    Ok(pages)
}

/// `which pdftoppm` without spawning a shell — returns the resolved path.
fn which_pdftoppm() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join("pdftoppm");
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

/// Embed PDF *pages* into the 3584-d 7b space. Each page becomes a page entity
/// (`KIND_PAGE`, `page::parent` → file, `page::index` → 1-based number) carrying
/// the shared `embeddings::attr_mm7b::embedding`, so `files similar --mm7b`
/// ranks pages and a hit resolves to "file X, page N". Idempotent: a file whose
/// pages already exist is skipped unless `--force`; page entity ids are intrinsic
/// (derived from parent+index), so re-runs merge rather than duplicate. Unique
/// PDF bytes are rendered+embedded once and the per-page vectors fan out to every
/// file entity that shares the content.
fn cmd_embed7b_pdf<P: TriblePattern>(
    pile: &mut FacultyStore,
    collection: Collection<SimpleArchive>,
    signer: &SigningKey,
    runtime: &tokio::runtime::Runtime,
    space: &P,
    reader: &impl BlobStoreGet,
    force: bool,
    dpi: u32,
    file_limit: usize,
    max_pages: usize,
    out: &mut Out<'_>,
) -> Result<()> {
    // Gather PDF file entities grouped by content hash (render once per unique
    // bytes, fan pages out to every sibling file entity).
    let mut groups: BTreeMap<String, (FileHandle, Vec<Id>)> = BTreeMap::new();
    for (eid, h) in find!(
        (eid: Id, h: FileHandle),
        pattern!(space, [{ ?eid @ metadata::tag: &KIND_FILE, file::content: ?h }])
    ) {
        if read_mime(space, reader, eid)?.as_deref() != Some("application/pdf") {
            continue;
        }
        groups
            .entry(handle_hex(h))
            .or_insert_with(|| (h, Vec::new()))
            .1
            .push(eid);
    }

    if groups.is_empty() {
        out.line(format!("(no PDF files to embed)"))?;
        return Ok(());
    }

    // A file entity is "done" if any page already references it as parent.
    let has_pages = |eid: Id| -> bool {
        exists!(
            (p: Id),
            pattern!(space, [{ ?p @ metadata::tag: &KIND_PAGE, page::parent: eid }])
        )
    };

    // Keep only groups with at least one file entity still needing work.
    let mut pending: Vec<(String, FileHandle, Vec<Id>)> = groups
        .into_iter()
        .filter_map(|(hash, (h, eids))| {
            let todo: Vec<Id> = if force {
                eids
            } else {
                eids.into_iter().filter(|e| !has_pages(*e)).collect()
            };
            (!todo.is_empty()).then_some((hash, h, todo))
        })
        .collect();

    if pending.is_empty() {
        out.line(format!(
            "All PDF files already have page embeddings (use --force to re-embed)."
        ))?;
        return Ok(());
    }
    pending.sort_by(|a, b| a.0.cmp(&b.0));
    if file_limit > 0 && pending.len() > file_limit {
        pending.truncate(file_limit);
    }
    let pending_pdfs = pending.len();

    let embedder = load_mm7b_opt()?;

    let mut change = Fragment::empty();
    let mut pdfs_done = 0usize;
    let mut pages_embedded = 0usize;
    let mut failed = 0usize;
    for (hash, content, eids) in &pending {
        let bytes: anybytes::Bytes = match reader.get::<anybytes::Bytes, _>(*content) {
            Ok(b) => b,
            Err(e) => {
                out.line(format!("  skip {hash}: read content failed: {e:?}"))?;
                failed += 1;
                continue;
            }
        };
        let pages = match render_pdf_pages(bytes.as_ref(), dpi, max_pages) {
            Ok(p) => p,
            Err(e) => {
                out.line(format!("  skip {hash}: render failed: {e:#}"))?;
                failed += 1;
                continue;
            }
        };
        if pages.is_empty() {
            out.line(format!("  skip {hash}: pdftoppm produced no pages"))?;
            failed += 1;
            continue;
        }
        let mut this_pages = 0usize;
        for (page_no, png) in &pages {
            let v = match mm7b_embed_image(&embedder, png) {
                Ok(v) => v,
                Err(e) => {
                    out.line(format!("  {hash} page {page_no}: embed failed: {e:#}"))?;
                    failed += 1;
                    continue;
                }
            };
            let idx_label = page_no.to_string();
            let handle: Mm7bHandle = change.put::<embeddings::Embedding3584, _>(v);
            for eid in eids {
                // Intrinsic page id from (parent, index): stable across re-runs.
                let page_id = entity! { _ @
                    page::parent: *eid,
                    page::index: idx_label.clone(),
                }
                .root()
                .expect("entity! derives a root id");
                change += entity! { ExclusiveId::force_ref(&page_id) @
                    metadata::tag: &KIND_PAGE,
                    page::parent: *eid,
                    page::index: idx_label.clone(),
                    embeddings::attr_mm7b::embedding: handle,
                };
            }
            this_pages += 1;
            pages_embedded += 1;
        }
        pdfs_done += 1;
        out.line(format!(
            "  {hash}: {this_pages} pages → {} entities",
            this_pages * eids.len()
        ))?;
    }

    if change.is_empty() {
        out.line(format!(
            "Nothing to commit (PDFs {pdfs_done}, failed {failed})."
        ))?;
        return Ok(());
    }

    pile.commit(collection, signer, change)
        .context("commit Files PDF page embeddings")?;
    ensure_files_after_commit(pile, collection, signer, runtime)?;

    out.line(format!(
        "7b-embedded {pages_embedded} pages across {pdfs_done} PDFs (of {pending_pdfs} pending){}",
        if failed > 0 {
            format!(", {failed} failures")
        } else {
            String::new()
        },
    ))?;
    Ok(())
}

/// Semantic nearest-neighbour search over the derived Files indexes.
///
/// Two indexes, one per model ([`SemanticModel`]): the image index holds a
/// row for every distinct content whose bytes are an image, the text index
/// for every PDF text layer, UTF-8 and HTML text, all in the one nomic space
/// and every row keyed by its content handle. A text query goes through the
/// text model's query side; a file query embeds that file's own bytes the way
/// the indexes did, image or text by content. Each index's floor is its own
/// constraint, because text-to-text cosines in this space sit near 0.7 and
/// text-to-image near 0.07. One `find!` unions the two thresholds and joins
/// the Files facts on the content handle with a free attribute, so a hit is a
/// content and every entity that holds it; the optional `--tag` filter, the
/// hybrid join that separates real forms from mascots, is part of the same
/// query. A mail attachment saved three times is three entities over one
/// content and one hit. The ranking and the per-kind limit are presentation
/// of that answer: the group whose best hit scores higher is printed first,
/// and `kind` queries one index only. With `mm7b`, the query and candidates
/// live in the 3584-d nomic-7b space (`attr_mm7b::embedding`, populated by
/// `files embed7b`) instead.
fn cmd_similar<P: TriblePattern>(
    store: &mut FacultyStore,
    collection: Collection<SimpleArchive>,
    signer: &SigningKey,
    runtime: &std::sync::Arc<tokio::runtime::Runtime>,
    space: &P,
    snapshot: &FacultySnapshot,
    options: &SimilarityOptions<'_>,
    out: &mut Out<'_>,
) -> Result<()> {
    let reader = AcquiringReader::new(snapshot.clone(), runtime.clone());
    if options.mm7b {
        return cmd_similar_mm7b(
            space,
            &reader,
            options.id,
            options.text,
            options.floor,
            options.limit,
            options.tags,
            out,
        );
    }
    #[cfg(not(feature = "local-embed"))]
    {
        let _ = (store, collection, signer, runtime);
        bail!("`files similar` needs the embedders — rebuild with --features local-embed");
    }
    #[cfg(feature = "local-embed")]
    {
        let (query_vec, query_contents, label) = similarity_query(space, &reader, &reader, options)?;

        // The indexes as they stand: a query is a read and never waits on the
        // GPU. `files add` embeds the file it just saved, and any earlier
        // file of this key's with no row at all, and `files index` every
        // file without a usable row, whoever saved it, on the canonical
        // compute; elsewhere the rows arrive by replication. Files
        // with no row yet (see SEMANTIC_COMPUTE) are counted rather than
        // hidden.
        let _ = (signer, runtime);
        let kinds: Vec<Kind> = Kind::ALL
            .into_iter()
            .filter(|kind| options.kind.is_none_or(|wanted| wanted == *kind))
            .collect();
        let mut targets = Vec::with_capacity(kinds.len());
        for kind in kinds {
            targets.push((
                kind,
                semantic_target(store, collection, kind, SEMANTIC_COMPUTE, &reader)?,
            ));
        }
        let snapshot = AcquiringReader::new(
            store.snapshot().context("freeze the Files semantic index")?, runtime.clone(),
        );
        // One reconstruction scan per index; an index not asked about scores
        // nothing and its branch of the query below is empty.
        let mut image_cosines = ReconstructedCosines::default();
        let mut text_cosines = ReconstructedCosines::default();
        for (kind, target) in &targets {
            let index = snapshot
                .collection_acquiring(*target)
                .with_context(|| format!("observe the Files {kind} index"))?
                .view::<NvFp4CosineIndex<embeddings::Embedding768>>()
                .with_context(|| format!("read the Files {kind} index"))?;
            let unindexed = crate::storage::underived(&snapshot, collection, *target)
                .with_context(|| format!("count Files commits without {kind} rows"))?;
            if unindexed > 0 {
                out.line(semantic_lag_note(*kind, unindexed))?;
            }
            if std::env::var_os("SEMANTIC_TRACE").is_some() {
                eprintln!(
                    "semantic {kind} index {}: {} segment(s), {} row(s) read",
                    collection_hex(target.handle()),
                    index.segment_count(),
                    index.len()
                );
                for (handle, rows) in index.segments() {
                    eprintln!("semantic segment {} {rows}", hex::encode(handle));
                }
            }
            let cosines = index
                .reconstructed_cosines(&query_vec)
                .map_err(|error| anyhow::anyhow!("search the Files {kind} index: {error}"))?;
            match kind {
                Kind::Image => image_cosines = cosines,
                Kind::Text => text_cosines = cosines,
            }
        }
        if image_cosines.is_empty() && text_cosines.is_empty() {
            bail!("the Files semantic index has no rows yet");
        }

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
        let holders = similar_contents(
            space,
            (&image_cosines, f64::from(options.floor_for(Kind::Image))),
            (&text_cosines, f64::from(options.floor_for(Kind::Text))),
            &tags,
        );

        // Presentation: each kind ranked by cosine, the query's own contents
        // left out, at most `limit` per kind. A content is in the index of
        // the model that read it, so its kind is where its score is.
        let mut image_hits: Vec<(f64, FileHandle)> = Vec::new();
        let mut text_hits: Vec<(f64, FileHandle)> = Vec::new();
        for content in holders.keys() {
            if query_contents.contains(content) {
                continue;
            }
            if let Some(cos) = image_cosines.cosine(content) {
                image_hits.push((cos, *content));
            } else if let Some(cos) = text_cosines.cosine(content) {
                text_hits.push((cos, *content));
            }
        }
        let mut groups: Vec<(&str, Vec<(f64, FileHandle)>)> = Vec::new();
        for (group, mut hits) in [("Images", image_hits), ("Texts", text_hits)] {
            if hits.is_empty() {
                continue;
            }
            hits.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
            hits.truncate(options.limit);
            groups.push((group, hits));
        }
        let floors = match options.kind {
            Some(kind) => format!("cos ≥ {}", options.floor_for(kind)),
            None => format!(
                "image cos ≥ {}, text cos ≥ {}",
                options.floor_for(Kind::Image),
                options.floor_for(Kind::Text)
            ),
        };
        if groups.is_empty() {
            out.line(format!("no files similar to {label} above {floors}"))?;
            return Ok(());
        }
        // The group whose best hit scores higher first: same-modality hits lead.
        groups.sort_by(|a, b| b.1[0].0.total_cmp(&a.1[0].0));
        out.line(format!("Similar to {label} ({floors}):"))?;
        for (group, hits) in &groups {
            let indent = if options.kind.is_none() {
                out.line(format!("  {group}:"))?;
                "    "
            } else {
                "  "
            };
            for (cos, content) in hits {
                let held = &holders[content];
                let eid = *held.first().expect("every hit has a holder");
                let name = read_name(space, &reader, eid)?.unwrap_or_else(|| "?".into());
                let mime = read_mime(space, &reader, eid)?.unwrap_or_else(|| "?".into());
                let hash = handle_hex(*content);
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
                    "{indent}{cos:.3}  {name}  ({mime})  {hash}{tagstr}{others}"
                ))?;
            }
        }
        Ok(())
    }
}

/// The query vector and a label, from either a text string (cross-modal, the
/// text model's query side) or a query file's bytes, embedded the way the
/// indexes embed them (image or text by content). A file query also names its
/// own contents, which its answer leaves out.
#[cfg(feature = "local-embed")]
fn similarity_query<P: TriblePattern, R: triblespace::core::repo::StoreRead>(
    space: &P,
    reader: &impl BlobStoreGet,
    models: &R,
    options: &SimilarityOptions<'_>,
) -> Result<(Vec<f32>, std::collections::BTreeSet<FileHandle>, String)> {
    let selector = match (options.text, options.id) {
        (Some(text), _) => {
            let vector = crate::nomic::load_text_embedder_in(models)?.embed_query(text)?;
            return Ok((vector, Default::default(), format!("{text:?}")));
        }
        (None, Some(selector)) => selector,
        (None, None) => bail!("give a file id/hash, or --text \"a query\""),
    };
    let eid = file_capability::resolve_selector(space, selector)?;
    let h = content_handle_of(space, eid).ok_or_else(|| {
        anyhow::anyhow!("that entity has no content bytes to embed; query with --text instead")
    })?;
    let bytes: anybytes::Bytes = reader
        .get::<anybytes::Bytes, _>(h)
        .context("read the query file's bytes")?;
    let name = read_name(space, reader, eid)?.unwrap_or_else(|| "?".into());
    let vector = match classify(bytes.as_ref()) {
        Content::Image => crate::nomic::load_vision_embedder_in(models)?
            .embed_image(bytes.as_ref())
            .context("embed the query image")?,
        Content::Pdf(text) | Content::Text(text) => crate::nomic::load_text_embedder_in(models)?
            .embed_document(&text)
            .context("embed the query document")?,
        Content::Other => {
            bail!(
                "{name} is neither an image nor text the indexes embed; query with --text instead"
            )
        }
    };
    let own = find!(
        content: FileHandle,
        pattern!(space, [{ eid @ file::content: ?content }])
    )
    .collect();
    Ok((vector, own, name))
}

/// Every content whose cosine clears its own index's floor, with every
/// entity that holds it under any attribute and carries every one of `tags`:
/// one query over both indexes and the Files facts. The index a content is
/// in says which model read it, so each index answers with its own floor and
/// the two thresholds are a union on the content variable. Holders come from
/// the source through the content handle itself, so a content held by three
/// entities is one key with three holders.
#[cfg(feature = "local-embed")]
fn similar_contents<P: TriblePattern>(
    space: &P,
    (images, image_floor): (&ReconstructedCosines, f64),
    (texts, text_floor): (&ReconstructedCosines, f64),
    tags: &[Inline<inlineencodings::ShortString>],
) -> BTreeMap<FileHandle, std::collections::BTreeSet<Id>> {
    use triblespace::core::query::Constraint;

    type ContentEncoding = inlineencodings::Handle<blobencodings::RawBytes>;
    let tag_attribute: Inline<inlineencodings::GenId> = file::tag.id().to_inline();
    let mut holders: BTreeMap<FileHandle, std::collections::BTreeSet<Id>> = BTreeMap::new();
    for (content, holder) in find!(
        (content: FileHandle, holder: Id),
        temp!(
            (attribute),
            and!(
                or!(
                    images.similar_to::<ContentEncoding>(content, image_floor),
                    texts.similar_to::<ContentEncoding>(content, text_floor),
                ),
                space.pattern(holder, attribute, content),
                IntersectionConstraint::new(
                    tags.iter()
                        .map(|tag| {
                            Box::new(space.pattern(holder, tag_attribute, *tag))
                                as Box<dyn Constraint + Send + Sync>
                        })
                        .collect()
                ),
            )
        )
    ) {
        holders.entry(content).or_default().insert(holder);
    }
    holders
}

/// Nearest-neighbour search in the nomic-embed-multimodal-7b 3584-d space.
/// Same shape as [`cmd_similar`] but over `attr_mm7b::embedding`: a text query
/// is embedded with the 7b's query-side path (text→image recall), a file query
/// reuses that file's stored 7b vector (image→image).
fn cmd_similar_mm7b<P: TriblePattern>(
    space: &P,
    reader: &impl BlobStoreGet,
    id: Option<&str>,
    text: Option<&str>,
    floor: f32,
    limit: usize,
    filter_tags: &[String],
    out: &mut Out<'_>,
) -> Result<()> {
    let (query_vec, query_eid, label): (Vec<f32>, Option<Id>, String) = match (text, id) {
        (Some(t), _) => {
            let embedder = load_mm7b_opt()?;
            (mm7b_embed_query(&embedder, t)?, None, format!("{t:?}"))
        }
        (None, Some(idstr)) => {
            let eid = file_capability::resolve_selector(space, idstr)?;
            let h: Mm7bHandle = find!(
                (h: Mm7bHandle),
                pattern!(space, [{ eid @ embeddings::attr_mm7b::embedding: ?h }])
            )
            .map(|(h,)| h)
            .next()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "that file has no 7b embedding — run `files embed-7b` first, \
                     or query with --text instead"
                )
            })?;
            let name = read_name(space, reader, eid)?.unwrap_or_else(|| "?".into());
            (read_embedding_3584(reader, h)?, Some(eid), name)
        }
        (None, None) => bail!("give a file id/hash, or --text \"a query\""),
    };

    let pairs: Vec<(Id, Mm7bHandle)> = find!(
        (eid: Id, h: Mm7bHandle),
        pattern!(space, [{ ?eid @ embeddings::attr_mm7b::embedding: ?h }])
    )
    .collect();
    if pairs.is_empty() {
        bail!("no 7b-embedded files yet — run `files embed-7b` first");
    }

    let mut vec_pairs: Vec<(Id, Vec<f32>)> = Vec::with_capacity(pairs.len());
    for (eid, h) in &pairs {
        vec_pairs.push((*eid, read_embedding_3584(reader, *h)?));
    }
    let ranked = embeddings::nearest(&vec_pairs, &query_vec, floor)?;

    let mut rows: Vec<(f32, Id)> = Vec::new();
    for (cos, eid) in ranked {
        if Some(eid) == query_eid {
            continue;
        }
        if !filter_tags.is_empty() {
            let tags = tags_of(space, eid);
            if !filter_tags.iter().all(|ft| tags.iter().any(|t| t == ft)) {
                continue;
            }
        }
        rows.push((cos, eid));
    }
    rows.truncate(limit);

    if rows.is_empty() {
        out.line(format!(
            "no files similar to {label} above cos {floor} (7b space)"
        ))?;
        return Ok(());
    }
    out.line(format!("Similar to {label} (7b space, cos ≥ {floor}):"))?;
    for (cos, eid) in &rows {
        // A page hit resolves to its parent file (name/mime/hash) + page number.
        let (display_eid, page_suffix) = match read_page(space, *eid) {
            Some((parent, idx)) => (parent, format!("  page {idx}")),
            None => (*eid, String::new()),
        };
        let name = read_name(space, reader, display_eid)?.unwrap_or_else(|| "?".into());
        let mime = read_mime(space, reader, display_eid)?.unwrap_or_else(|| "?".into());
        let hash = content_handle_of(space, display_eid)
            .map(handle_hex)
            .unwrap_or_default();
        let tags = tags_of(space, display_eid);
        let tagstr = if tags.is_empty() {
            String::new()
        } else {
            format!("  [{}]", tags.join(", "))
        };
        out.line(format!(
            "  {cos:.3}  {name}{page_suffix}  ({mime})  {hash}{tagstr}"
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
    /// The cosine floor of every kind that has no floor of its own.
    pub floor: f32,
    /// The image index's floor, when it differs from `floor`.
    pub image_floor: Option<f32>,
    /// The text index's floor, when it differs from `floor`.
    pub text_floor: Option<f32>,
    pub limit: usize,
    pub tags: &'a [String],
    /// Rank only this kind; both kinds, as two groups, when absent.
    pub kind: Option<Kind>,
    pub mm7b: bool,
}

impl SimilarityOptions<'_> {
    /// The floor the index of `kind` answers with.
    pub fn floor_for(&self, kind: Kind) -> f32 {
        match kind {
            Kind::Image => self.image_floor,
            Kind::Text => self.text_floor,
        }
        .unwrap_or(self.floor)
    }
}

/// One of the two kinds the semantic index ranks separately: each is its own
/// index, embedded by its own model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Image,
    Text,
}

impl Kind {
    /// Both kinds, images first.
    pub const ALL: [Kind; 2] = [Kind::Image, Kind::Text];
}

impl std::fmt::Display for Kind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Kind::Image => "image",
            Kind::Text => "text",
        })
    }
}

impl std::str::FromStr for Kind {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "image" | "images" => Ok(Kind::Image),
            "text" | "texts" => Ok(Kind::Text),
            other => bail!("unknown kind {other:?}: image or text"),
        }
    }
}

pub struct EmbeddingOptions {
    pub force: bool,
    pub pdf: bool,
    pub dpi: u32,
    pub limit: usize,
    pub max_pages: usize,
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
        with_files_read(&self.storage, |store, facts, snapshot, rt| {
            load_export(store, rt, facts, snapshot, id)
        })
    }

    pub fn view(
        &self,
        id: &str,
        options: &super::presentation::ViewOptions,
    ) -> Result<crate::out::Part> {
        options.validate()?;
        let selected = with_files_read(&self.storage, |store, facts, snapshot, rt| {
            load_view(store, rt, facts, snapshot, id)
        })?;
        super::presentation::present(selected.bytes, &selected.mime_type, options)
    }

    pub fn extract(&self, id: &str, destination: Option<&Path>) -> Result<Extraction> {
        with_files_read(&self.storage, |store, facts, snapshot, rt| {
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
    /// With `local-embed`, raster images also require the configured nomic-vision model
    /// and runtime for the same automatic embedding used by CLI image imports.
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
        with_files_read(&self.storage, |store, facts, snapshot, rt| {
            rt.block_on(read(store, snapshot, |reader| {
                cmd_list(facts, reader, tags, mime)
            }))
        })
    }

    pub fn show(&self, id: &str) -> Result<String> {
        with_files_read(&self.storage, |store, facts, snapshot, rt| {
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
        with_files_read(&self.storage, |store, facts, snapshot, rt| {
            rt.block_on(read(store, snapshot, |reader| {
                cmd_search(facts, reader, query)
            }))
        })
    }

    pub fn similar(&self, options: &SimilarityOptions<'_>, out: &mut Out<'_>) -> Result<()> {
        anyhow::ensure!(
            options.id.is_some() ^ options.text.is_some(),
            "provide exactly one of id or text"
        );
        for floor in [Some(options.floor), options.image_floor, options.text_floor]
            .into_iter()
            .flatten()
        {
            anyhow::ensure!(
                floor.is_finite() && (0.0..=1.0).contains(&floor),
                "floor must be between 0 and 1"
            );
        }
        with_files_view(
            &self.storage,
            |store, collection, signer, facts, snapshot, runtime| {
                cmd_similar(
                    store, collection, signer, runtime, facts, snapshot, options, out,
                )
            },
        )
    }

    /// Maintain the semantic indexes over every stored file (see [`cmd_index`]).
    pub fn index(&self, out: &mut Out<'_>) -> Result<()> {
        with_files_store(&self.storage, |store, collection, signer, runtime| {
            cmd_index(store, collection, signer, runtime, out)
        })
    }

    pub fn golden(&self, publish: bool, out: &mut Out<'_>) -> Result<()> {
        #[cfg(not(feature = "local-embed"))]
        {
            let _ = (publish, out);
            bail!("`files golden` needs the embedders — rebuild with --features local-embed");
        }
        #[cfg(feature = "local-embed")]
        with_files_store(&self.storage, |store, _collection, signer, runtime| {
            cmd_golden(store, signer, runtime, publish, out)
        })
    }

    pub fn embed7b(&self, options: &EmbeddingOptions, out: &mut Out<'_>) -> Result<()> {
        anyhow::ensure!(options.dpi > 0, "dpi must be positive");
        // Model-backed operations retain their separate inference/acquisition
        // boundaries; never retry the whole operation after partial publication.
        with_files_view(
            &self.storage,
            |store, collection, signer, facts, snapshot, runtime| {
                let reader = AcquiringReader::new(snapshot.clone(), runtime.clone());
                if options.pdf {
                    cmd_embed7b_pdf(
                        store,
                        collection,
                        signer,
                        runtime,
                        facts,
                        &reader,
                        options.force,
                        options.dpi,
                        options.limit,
                        options.max_pages,
                        out,
                    )
                } else {
                    cmd_embed7b(
                        store,
                        collection,
                        signer,
                        runtime,
                        facts,
                        &reader,
                        options.force,
                        out,
                    )
                }
            },
        )
    }

    pub fn imports(&self) -> Result<String> {
        with_files_read(&self.storage, |store, facts, snapshot, rt| {
            rt.block_on(read(store, snapshot, |reader| cmd_imports(facts, reader)))
        })
    }

    pub fn tree(&self, id: &str, depth: Option<usize>) -> Result<String> {
        with_files_read(&self.storage, |store, facts, snapshot, rt| {
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
        with_files_read(&self.storage, |_, facts, _, _| {
            Ok(selectors
                .iter()
                .map(|selector| file_capability::resolve_reference(facts, selector))
                .collect())
        })
    }

    pub fn diff(&self, left: &str, right: &str) -> Result<String> {
        with_files_read(&self.storage, |store, facts, snapshot, rt| {
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

    /// The Files similarity query over two content-keyed indexes: each kind
    /// answers with its own floor, holders come through the content handle
    /// under any attribute, a content several entities hold is one key with
    /// every holder, and the tag filter is part of the same query.
    #[cfg(feature = "local-embed")]
    #[test]
    fn similar_contents_joins_both_indexes_to_the_files_facts() {
        use triblespace::core::collection::{AdmissionPolicy, CollectionPolicy};
        use triblespace::core::trible::Trible;
        use triblespace_search::nvfp4::NvFp4EmbeddingAttribute;

        // Stand-in rows without a model: content blobs that are themselves
        // 768-d vectors, so the exact NVFP4 mapping over `file::content`
        // keys its rows by the content handle, as the semantic index does.
        let vector = |axis: usize, other: Option<(usize, f32)>| {
            let mut v = vec![0.0f32; embeddings::DIM];
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
        let mut put = |v: Vec<f32>| -> FileHandle {
            store
                .put::<embeddings::Embedding768, _>(v)
                .unwrap()
                .transmute()
        };
        let exact = put(vector(0, None));
        let close = put(vector(0, Some((1, 0.2))));
        let far = put(vector(5, None));
        let text = put(vector(0, Some((2, 0.2))));

        let entity = |byte: u8| Id::new([byte; 16]).unwrap();
        let mut facts = TribleSet::new();
        let mut images = TribleSet::new();
        let mut texts = TribleSet::new();
        for (holder, content, image) in [
            (1, exact, true),
            (2, exact, true),
            (3, close, true),
            (4, far, true),
            (5, text, false),
        ] {
            let fact = Trible::force(&entity(holder), &file::content.id(), &content);
            facts.insert(&fact);
            if image {
                images.insert(&fact);
            } else {
                texts.insert(&fact);
            }
        }
        // The text content held under another attribute by another entity.
        facts.insert(&Trible::force(
            &entity(6),
            &file::name.id(),
            &text.transmute::<inlineencodings::Handle<blobencodings::UTF8String>>(),
        ));
        let tagged = entity(1);
        facts += TribleSet::from(entity! { ExclusiveId::force_ref(&tagged) @ file::tag: "form" });

        let mut index = |name: &str, rows: TribleSet| {
            let source = store.collection(name, policy.clone()).unwrap();
            let target = store
                .derive::<NvFp4CosineSet<embeddings::Embedding768>>(
                    source,
                    NvFp4EmbeddingAttribute::new(file::content.id(), embeddings::DIM).unwrap(),
                    policy.clone(),
                )
                .unwrap();
            store.commit(source, &key, Fragment::from(rows)).unwrap();
            let snapshot = pollster::block_on(store.maintain(target, &key)).unwrap();
            snapshot
                .collection(target)
                .unwrap()
                .view::<NvFp4CosineIndex<embeddings::Embedding768>>()
                .unwrap()
                .reconstructed_cosines(&vector(0, None))
                .unwrap()
        };
        let image_cosines = index("images", images);
        let text_cosines = index("texts", texts);

        let holders = |image_floor: f64, text_floor: f64, tags: &[&str]| {
            let tags: Vec<Inline<inlineencodings::ShortString>> = tags
                .iter()
                .map(|tag| tag.try_to_inline().unwrap())
                .collect();
            similar_contents(
                &facts,
                (&image_cosines, image_floor),
                (&text_cosines, text_floor),
                &tags,
            )
        };
        let set = |bytes: &[u8]| {
            bytes
                .iter()
                .map(|byte| entity(*byte))
                .collect::<BTreeSet<_>>()
        };

        assert_eq!(
            holders(0.9, 0.5, &[]),
            BTreeMap::from([
                (exact, set(&[1, 2])),
                (close, set(&[3])),
                (text, set(&[5, 6]))
            ])
        );
        // Each index has its own floor.
        assert_eq!(
            holders(0.99, 0.99, &[]),
            BTreeMap::from([(exact, set(&[1, 2]))])
        );
        assert_eq!(
            holders(1.1, 0.5, &[]),
            BTreeMap::from([(text, set(&[5, 6]))])
        );
        // The tag is asked of the holder, inside the same query.
        assert_eq!(
            holders(0.9, 0.5, &["form"]),
            BTreeMap::from([(exact, set(&[1]))])
        );
        assert!(holders(0.9, 0.5, &["form", "other"]).is_empty());
        // The score that ranks a hit is the cosine of the index that holds it.
        assert!(image_cosines.cosine(&exact).unwrap() > 0.999);
        assert_eq!(text_cosines.cosine(&exact), None);
    }

    #[cfg(feature = "local-embed")]
    #[test]
    fn semantic_targets_keep_the_model_observation_used_by_the_operation() {
        use triblespace::core::collection::{AdmissionPolicy, CollectionPolicy};

        let fixture = TestPile::new();
        let publisher = SigningKey::from_bytes(&[0x76; 32]);
        let mut pile = Pile::open(&fixture.path).unwrap();
        for fragment in [
            native_model_fragment(crate::nomic::NOMIC_TEXT_MODEL, "text.weight", 1.0),
            native_model_fragment(crate::nomic::NOMIC_VISION_MODEL, "vision.weight", 2.0),
            native_tokenizer_fragment(crate::nomic::NOMIC_TEXT_MODEL, WORDPIECE),
        ] {
            mary::model_collection::publish_model_fragment(&mut pile, &publisher, fragment)
                .unwrap();
        }
        pile.close().unwrap();

        let storage = Storage::new(fixture.path.clone(), None);
        storage
            .with_store(|store, _, runtime| {
                let policy = CollectionPolicy::new(AdmissionPolicy::Open, AdmissionPolicy::Open);
                let files = store.collection("files", policy)?;
                let frozen = AcquiringReader::new(store.snapshot()?, runtime.clone());
                let models = crate::nomic::index_models_in(&frozen)?;
                let before = semantic_target(store, files, Kind::Text, SEMANTIC_COMPUTE, &frozen)?;

                // A later admitted model would make a fresh selection ambiguous.
                // It must not replace the model used by this command's query/check.
                let model_collection =
                    mary::model_collection::model_graph_collections_in(&frozen)?[0];
                store.commit(
                    model_collection,
                    &publisher,
                    native_model_fragment(crate::nomic::NOMIC_TEXT_MODEL, "later.weight", 3.0),
                )?;
                let after = semantic_target(store, files, Kind::Text, SEMANTIC_COMPUTE, &frozen)?;
                assert_eq!(before, after);
                assert_eq!(models, crate::nomic::index_models_in(&frozen)?);
                let current = AcquiringReader::new(store.snapshot()?, runtime.clone());
                assert!(crate::nomic::index_models_in(&current).is_err());
                Ok(())
            })
            .unwrap();
    }

    #[cfg(feature = "local-embed")]
    #[test]
    fn semantic_descriptor_ignores_observations_extra_models_and_support_packaging() {
        use triblespace::core::collection::{AdmissionPolicy, CollectionPolicy, DeriveMapping};

        let split = TestPile::new();
        let packed = TestPile::new();
        let signer = SigningKey::from_bytes(&[0x72; 32]);
        let text = native_model_fragment(crate::nomic::NOMIC_TEXT_MODEL, "text.weight", 1.0);
        let vision = native_model_fragment(crate::nomic::NOMIC_VISION_MODEL, "vision.weight", 2.0);
        let tokenizer = native_tokenizer_fragment(crate::nomic::NOMIC_TEXT_MODEL, WORDPIECE);
        let mut split_pile = Pile::open(&split.path).unwrap();
        for fragment in [text.clone(), vision.clone(), tokenizer.clone()] {
            mary::model_collection::publish_model_fragment(&mut split_pile, &signer, fragment)
                .unwrap();
        }
        let frozen = split_pile.snapshot().unwrap();
        let before = semantic_index(&frozen, Kind::Text, SEMANTIC_COMPUTE).unwrap();
        let SemanticModel::Text {
            root: selected_text,
            ..
        } = before.model
        else {
            panic!("a text index names a text model");
        };
        let other_root = fucid();
        let mut additions = entity! {
            mary::format::attrs::model_root: selected_text,
            crate::nomic::golden::text_embedding: vec![1.0f32; embeddings::DIM],
        };
        additions += entity! {
            mary::format::attrs::model_root: &other_root,
            crate::nomic::golden::text_embedding: vec![0.0f32; embeddings::DIM],
        };
        // Historical root-owned observations also leave the reference tuple
        // alone. They remain in place; this test does not reinterpret their ids.
        additions += entity! { ExclusiveId::force_ref(&selected_text) @
            crate::nomic::golden::text_embedding: vec![1.0f32; embeddings::DIM],
        };
        additions += native_model_fragment("another/model", "other.weight", 3.0);
        additions += native_tokenizer_fragment("another/tokenizer", LATER_WORDPIECE);
        mary::model_collection::publish_model_fragment(&mut split_pile, &signer, additions.clone())
            .unwrap();
        let widened = split_pile.snapshot().unwrap();
        let after = semantic_index(&widened, Kind::Text, SEMANTIC_COMPUTE).unwrap();
        assert_eq!(before, after);
        assert_eq!(before.fragment(), after.fragment());

        // A separate replica has the same roots and annotations in one member
        // instead of four. Collection identity stays the same; support does not.
        let mut packed_pile = Pile::open(&packed.path).unwrap();
        mary::model_collection::publish_model_fragment(
            &mut packed_pile,
            &signer,
            text + vision + tokenizer + additions,
        )
        .unwrap();
        let repackaged = packed_pile.snapshot().unwrap();
        let repackaged_index = semantic_index(&repackaged, Kind::Text, SEMANTIC_COMPUTE).unwrap();
        assert_eq!(before, repackaged_index);
        let split_models = mary::model_collection::snapshot_model_collection_in(&widened).unwrap();
        let packed_models =
            mary::model_collection::snapshot_model_collection_in(&repackaged).unwrap();
        assert_eq!(split_models.support().len(), 4);
        assert_eq!(packed_models.support().len(), 1);
        assert_eq!(split_models.facts(), packed_models.facts());

        let root = signer.verifying_key();
        let policy =
            CollectionPolicy::new(AdmissionPolicy::direct(root), AdmissionPolicy::direct(root));
        let split_source = split_pile.collection("files", policy.clone()).unwrap();
        let packed_source = packed_pile.collection("files", policy.clone()).unwrap();
        let first_target = split_pile
            .derive_with(split_source, before, policy.clone())
            .unwrap();
        let later_target = split_pile
            .derive_with(split_source, after, policy.clone())
            .unwrap();
        let repackaged_target = packed_pile
            .derive_with(packed_source, repackaged_index, policy)
            .unwrap();
        assert_eq!(first_target, later_target);
        assert_eq!(first_target, repackaged_target);

        // Tokenizer selection remains explicit even with an unrelated second
        // tokenizer in the same collection.
        let selected = crate::nomic::index_models_in(&repackaged).unwrap();
        let tokenizer = mary::selection::load_tokenizer_from_graph(
            packed_models.facts(),
            &repackaged,
            mary::selection::TokenizerSelector::Root(selected.tokenizer_root),
        )
        .unwrap();
        assert_eq!(tokenizer.token_to_id("hello"), Some(1));
        assert_eq!(tokenizer.token_to_id("later"), None);
        split_pile.close().unwrap();
        packed_pile.close().unwrap();
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
        with_files_read(&storage, |_, space, _reader, _rt| {
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
        with_files_read(&owner, |_, space, _, _| {
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
            .with_store(|store, _, runtime| {
                let collection = open(store, DEFAULT_SCOPE_ID, authority)
                    .context("open the owner's Files collection")?;
                files_view_in(store, &[collection], runtime, |_, space, _, _| {
                    assert_eq!(file_ids(space), BTreeSet::from([first_id, second_id]));
                    Ok(())
                })
            })
            .unwrap();

        // Once the worker carries the second commit, both readers still see
        // both.
        with_files_store(&owner, |store, collection, signer, _| {
            crate::storage::carry_facts(store, collection, signer);
            Ok(())
        })
        .unwrap();
        with_files_read(&owner, |_, space, _, _| {
            assert_eq!(file_ids(space), BTreeSet::from([first_id, second_id]));
            Ok(())
        })
        .unwrap();
        reader
            .with_store(|store, _, runtime| {
                let collection = open(store, DEFAULT_SCOPE_ID, authority)
                    .context("open the owner's Files collection")?;
                files_view_in(store, &[collection], runtime, |_, space, _, _| {
                    assert_eq!(file_ids(space), BTreeSet::from([first_id, second_id]));
                    Ok(())
                })
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
        with_files_read(&storage, |store, space, reader, rt| {
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

        with_files_read(&storage, |_, space, _reader, _rt| {
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

    /// Review finding, 2026-09-28 (test gap): on a machine outside the
    /// compute class of an index, `files index` embeds nothing -- no golden
    /// check, no model load, no row of its own -- and carries the rows a
    /// machine of the class derived into this key's own merge. The class
    /// here is one no machine is in, which is how every machine but the
    /// canonical one sees the real index; the rows stand in for a model's
    /// with the exact NVFP4 mapping over contents that are themselves
    /// vectors.
    #[cfg(feature = "local-embed")]
    #[test]
    fn files_index_outside_the_compute_class_carries_rows_and_embeds_nothing() {
        use triblespace::core::collection::{
            AdmissionPolicy, CollectionDerive, CollectionHandle, CollectionPolicy, CollectionRead,
            CollectionRecord, CollectionRecordSelector, CollectionStore,
        };
        use triblespace::core::trible::Trible;
        use triblespace_search::nvfp4::NvFp4EmbeddingAttribute;

        const ELSEWHERE: &str = "a-class-no-machine-is-in";
        let fixture = TestPile::new();
        let publisher = SigningKey::from_bytes(&[0x76; 32]);
        let mut pile = Pile::open(&fixture.path).unwrap();
        for fragment in [
            native_model_fragment(crate::nomic::NOMIC_TEXT_MODEL, "text.weight", 1.0),
            native_model_fragment(crate::nomic::NOMIC_VISION_MODEL, "vision.weight", 2.0),
            native_tokenizer_fragment(crate::nomic::NOMIC_TEXT_MODEL, WORDPIECE),
        ] {
            mary::model_collection::publish_model_fragment(&mut pile, &publisher, fragment)
                .unwrap();
        }
        pile.close().unwrap();

        let deriver = SigningKey::from_bytes(&[0x77; 32]);
        let storage = Storage::new(fixture.path.clone(), None);
        storage
            .with_store(|store, signer, runtime| {
                let policy = CollectionPolicy::new(AdmissionPolicy::Open, AdmissionPolicy::Open);
                let files = store.collection("files", policy.clone())?;
                for axis in 0..8u8 {
                    let mut vector = vec![0.0f32; embeddings::DIM];
                    vector[usize::from(axis)] = 1.0;
                    let content: FileHandle = store
                        .put::<embeddings::Embedding768, _>(vector)?
                        .transmute();
                    let mut facts = TribleSet::new();
                    facts.insert(&Trible::force(
                        &Id::new([axis + 1; 16]).unwrap(),
                        &file::content.id(),
                        &content,
                    ));
                    store.commit(files, &deriver, Fragment::from(facts))?;
                }
                let exact = store.derive::<NvFp4CosineSet<embeddings::Embedding768>>(
                    files,
                    NvFp4EmbeddingAttribute::new(file::content.id(), embeddings::DIM)?,
                    policy,
                )?;
                drop(runtime.block_on(store.ensure(exact, &deriver))?);
                let descriptors = AcquiringReader::new(store.snapshot()?, runtime.clone());
                let text = semantic_target(store, files, Kind::Text, ELSEWHERE, &descriptors)?;
                let records = |store: &mut FacultyStore, collection: CollectionHandle| {
                    store
                        .snapshot()
                        .unwrap()
                        .select_records(&BTreeSet::from([CollectionRecordSelector::Collection(
                            collection,
                        )]))
                        .unwrap()
                };
                // The rows the deriver computed, as leaves of the index.
                for record in records(store, exact.handle()) {
                    if let CollectionRecord::Derive(leaf) = record {
                        store.insert(CollectionRecord::Derive(CollectionDerive::sign(
                            &deriver,
                            text.handle(),
                            leaf.input(),
                            leaf.output(),
                        )))?;
                    }
                }

                let (targets, _, failures) =
                    maintain_semantic_on(store, files, signer, runtime, true, ELSEWHERE)?;
                assert!(failures.is_empty(), "{failures:?}");
                let signed_by_signer = |record: &CollectionRecord| match record {
                    CollectionRecord::Derive(leaf) => {
                        leaf.public_key().raw == signer.verifying_key().to_bytes()
                    }
                    _ => false,
                };
                for (kind, target) in targets {
                    let held = records(store, target.handle());
                    assert!(
                        !held.iter().any(signed_by_signer),
                        "{kind}: nothing embedded"
                    );
                    let merges: Vec<_> = held
                        .iter()
                        .filter_map(|record| match record {
                            CollectionRecord::Merge(merge) => Some(*merge),
                            _ => None,
                        })
                        .collect();
                    if kind == Kind::Text {
                        assert_eq!(target.handle(), text.handle());
                        assert_eq!(merges.len(), 1, "the eight rows are carried");
                        assert_eq!(merges[0].inputs().len(), 8);
                        assert_eq!(
                            merges[0].public_key().raw,
                            signer.verifying_key().to_bytes()
                        );
                    } else {
                        assert!(held.is_empty(), "{kind}: no rows, nothing to carry");
                    }
                }
                Ok(())
            })
            .unwrap();
    }

    /// Review finding, 2026-09-28: one index failing -- the Image index,
    /// maintained first -- ended `files index` before the Text index was
    /// derived or carried, and before either lag note was printed. Now both
    /// are maintained and reported, and the failure comes after.
    #[cfg(feature = "local-embed")]
    #[test]
    fn a_failing_image_index_holds_back_neither_the_text_index_nor_the_lag_notes() {
        use triblespace::core::collection::{
            AdmissionPolicy, CollectionDerive, CollectionHandle, CollectionPolicy, CollectionRead,
            CollectionRecord, CollectionRecordSelector, CollectionStore,
        };
        use triblespace::core::inline::encodings::hash::Handle;
        use triblespace::core::trible::Trible;
        use triblespace_search::nvfp4::NvFp4EmbeddingAttribute;

        const ELSEWHERE: &str = "a-class-no-machine-is-in";
        let fixture = TestPile::new();
        let publisher = SigningKey::from_bytes(&[0x76; 32]);
        let mut pile = Pile::open(&fixture.path).unwrap();
        for fragment in [
            native_model_fragment(crate::nomic::NOMIC_TEXT_MODEL, "text.weight", 1.0),
            native_model_fragment(crate::nomic::NOMIC_VISION_MODEL, "vision.weight", 2.0),
            native_tokenizer_fragment(crate::nomic::NOMIC_TEXT_MODEL, WORDPIECE),
        ] {
            mary::model_collection::publish_model_fragment(&mut pile, &publisher, fragment)
                .unwrap();
        }
        pile.close().unwrap();

        let deriver = SigningKey::from_bytes(&[0x77; 32]);
        let storage = Storage::new(fixture.path.clone(), None);
        storage
            .with_store(|store, signer, runtime| {
                let policy = CollectionPolicy::new(AdmissionPolicy::Open, AdmissionPolicy::Open);
                let files = store.collection("files", policy.clone())?;
                for axis in 0..9u8 {
                    let mut vector = vec![0.0f32; embeddings::DIM];
                    vector[usize::from(axis)] = 1.0;
                    let content: FileHandle = store
                        .put::<embeddings::Embedding768, _>(vector)?
                        .transmute();
                    let mut facts = TribleSet::new();
                    facts.insert(&Trible::force(
                        &Id::new([axis + 1; 16]).unwrap(),
                        &file::content.id(),
                        &content,
                    ));
                    store.commit(files, &deriver, Fragment::from(facts))?;
                }
                let exact = store.derive::<NvFp4CosineSet<embeddings::Embedding768>>(
                    files,
                    NvFp4EmbeddingAttribute::new(file::content.id(), embeddings::DIM)?,
                    policy,
                )?;
                drop(runtime.block_on(store.ensure(exact, &deriver))?);
                let descriptors = AcquiringReader::new(store.snapshot()?, runtime.clone());
                let image = semantic_target(store, files, Kind::Image, ELSEWHERE, &descriptors)?;
                let text = semantic_target(store, files, Kind::Text, ELSEWHERE, &descriptors)?;
                let records = |store: &mut FacultyStore, collection: CollectionHandle| {
                    store
                        .snapshot()
                        .unwrap()
                        .select_records(&BTreeSet::from([CollectionRecordSelector::Collection(
                            collection,
                        )]))
                        .unwrap()
                };
                // Eight of the nine files have text rows. The image index has
                // leaves for the same eight whose outputs are no NVFP4 rows,
                // so its carry cannot join them.
                let rows: Vec<CollectionDerive> = records(store, exact.handle())
                    .into_iter()
                    .filter_map(|record| match record {
                        CollectionRecord::Derive(leaf) => Some(leaf),
                        _ => None,
                    })
                    .collect();
                assert_eq!(rows.len(), 9);
                for (n, row) in rows.iter().take(8).enumerate() {
                    store.insert(CollectionRecord::Derive(CollectionDerive::sign(
                        &deriver,
                        text.handle(),
                        row.input(),
                        row.output(),
                    )))?;
                    let garbage = store.put::<UnknownBlob, _>(anybytes::Bytes::from_source(
                        format!("not an NVFP4 row {n}").into_bytes(),
                    ))?;
                    store.insert(CollectionRecord::Derive(CollectionDerive::sign(
                        &deriver,
                        image.handle(),
                        row.input(),
                        Handle::<UnknownBlob>::to_hash(garbage),
                    )))?;
                }
                let merges = |store: &mut FacultyStore, collection: CollectionHandle| {
                    records(store, collection)
                        .into_iter()
                        .filter(|record| matches!(record, CollectionRecord::Merge(_)))
                        .count()
                };

                let mut lines = Vec::new();
                let mut emit = |part: crate::out::Part| {
                    if let crate::out::Part::Text { text } = part {
                        lines.push(text);
                    }
                    Ok(())
                };
                let result = index_on(
                    store,
                    files,
                    signer,
                    runtime,
                    &mut Out::new(&mut emit),
                    ELSEWHERE,
                );
                let error = format!("{:#}", result.unwrap_err());
                assert!(error.contains("maintain the Files image index"), "{error}");
                assert!(!error.contains("text index"), "{error}");
                assert_eq!(merges(store, text.handle()), 1, "the text rows are carried");
                assert_eq!(merges(store, image.handle()), 0);
                let printed = lines.concat();
                assert!(
                    printed.contains(&format!(
                        "Files text index {}",
                        collection_hex(text.handle())
                    )),
                    "{printed}"
                );
                for kind in Kind::ALL {
                    assert!(
                        printed.contains(&format!("no {kind} rows here yet")),
                        "{kind}: {printed}"
                    );
                }
                Ok(())
            })
            .unwrap();
    }
}
