//! The one model the faculties search with: WeMM-Embedding-9B, native CUDA
//! BF16 on a GB10, which places text, images and documents in one
//! 4096-dimensional space. Files, Memory and Wiki each derive a collection of
//! NVFP4 rows from their own source collection through
//! [`triblespace_search::semantic_wemm`], keyed by content handle, and join it
//! at the point of use with `find!`.
//!
//! The model lives in its own pile, outside the working pile, named by
//! `WEMM_PILE` together with its pinned assets (`WEMM_ASSETS`) and model root
//! (`WEMM_ROOT`). Binding aliases that pile's mapped pages straight into CUDA,
//! so the operator keeps its complete prefix unchanged until the process ends;
//! the checks below are diagnostics, not proof of that.

use crate::out::Out;
use crate::storage::{AcquiringReader, FacultyStore};
use anyhow::{bail, Context, Result};
use ed25519_dalek::SigningKey;
use mary::models::qwen3_5::input_codec::InputCodec;
use mary::models::qwen3_5::native::{device_identity, Assets, NativeWemm};
use mary::nn::cuda_bf16_alias::CudaBf16Aliases;
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use triblespace::core::blob::encodings::rawbytes::RawBytes;
use triblespace::core::blob::encodings::simplearchive::SimpleArchive;
use triblespace::core::blob::Blob;
use triblespace::core::collection::{
    Collection, CollectionHandle, CollectionOperationError, CollectionSnapshotExt,
    CollectionStoreExt, DeriveMapping,
};
use triblespace::core::inline::encodings::UnknownInline;
use triblespace::core::repo::{BlobStoreGet, BlobStorePut, StoreRead};
use triblespace::core::trible::Fragment;
use triblespace::prelude::*;
use triblespace_search::nvfp4::{NvFp4CosineIndex, NvFp4CosineSet, ReconstructedCosines};
use triblespace_search::schemas::Embedding;
use triblespace_search::semantic_wemm::{
    with_wemm_runtime, WemmIdentity, WemmIndex, WemmRuntime, DEFAULT_WINDOW_CAP,
    GB10_NATIVE_BF16_PROFILE,
};

type Index = WemmIndex<Embedding>;
type Target = Collection<NvFp4CosineSet<Embedding>>;

/// WeMM-Embedding-9B's BF16 tensors, one alias registration each.
const ROLES: usize = 759;

/// What a query asks about: text, or content bytes read the way the index
/// reads them (an image whole, the text of text, HTML or a PDF).
pub enum Query<'a> {
    Text(&'a str),
    Content(&'a [u8]),
}

/// One bound model, reused by every index and query of a process: a bind
/// costs 30 to 110 s. Deliberately not `Send`; it lends its runtime only to
/// synchronous work on this thread.
pub struct Session {
    runtime: Rc<RefCell<WemmRuntime>>,
    identity: WemmIdentity,
    model: PathBuf,
    backing: BackingIdentity,
    _source: mary::persist::ModelPileSource,
    assets: [Blob<RawBytes>; 3],
    descriptor: Blob<SimpleArchive>,
}

impl Session {
    /// Bind the model `WEMM_PILE`, `WEMM_ASSETS` and `WEMM_ROOT` name, for
    /// indexes in the pile at `writable`. The model pile holds one model
    /// collection, and that is the one selected.
    pub fn from_env(writable: &Path) -> Result<Self> {
        let path = |name: &str| {
            std::env::var_os(name)
                .map(PathBuf::from)
                .with_context(|| format!("{name} is not set; it names the WeMM model"))
        };
        let model = path("WEMM_PILE")?;
        let assets = path("WEMM_ASSETS")?;
        let root = std::env::var("WEMM_ROOT")
            .context("WEMM_ROOT is not set; it names the WeMM model root")?;
        let root = Id::from_hex(&root).context("WEMM_ROOT is not a 32-hex-digit id")?;
        let read = |name: &str| {
            std::fs::read(assets.join(name))
                .with_context(|| format!("read WeMM asset {}", assets.join(name).display()))
        };
        let (config, tokenizer, template) = (
            read("config.json")?,
            read("tokenizer.json")?,
            read("chat_template.jinja")?,
        );
        let pinned =
            Assets::from_bytes(&config, &tokenizer, &template).map_err(anyhow::Error::msg)?;
        let codec = InputCodec::from_assets(&tokenizer, &template).map_err(anyhow::Error::msg)?;
        let backing = backing(&model)?;
        distinct_files(&backing, writable)?;
        let source = mary::persist::read_model_pile_read_only(&model)?;
        let [contributing] = source.collections.as_slice() else {
            bail!(
                "{} holds {} model collections; the WeMM pile holds one",
                model.display(),
                source.collections.len()
            );
        };
        let collection = contributing.collection;
        let selected =
            mary::model_collection::snapshot_model_collection_for(&source.store, collection)?;
        anyhow::ensure!(
            find!((attribute: Id, value: Inline<UnknownInline>),
                selected.facts().pattern::<UnknownInline>(root.to_inline(), attribute, value)
            )
            .next()
            .is_some(),
            "WEMM_ROOT {root:X} is not stated by the model collection of {}",
            model.display()
        );
        let descriptor: Blob<SimpleArchive> = source.store.get(collection.handle())?;
        let device = Default::default();
        let observed = device_identity(&device).map_err(anyhow::Error::msg)?;
        anyhow::ensure!(
            observed == ("NVIDIA GB10".into(), 12, 1),
            "WeMM runs on an NVIDIA GB10 (compute 12.1); this is {observed:?}"
        );
        let mut aliases = CudaBf16Aliases::new(device, ROLES).map_err(anyhow::Error::msg)?;
        let started = std::time::Instant::now();
        let mut roles = 0usize;
        // SAFETY: the operator who names WEMM_PILE keeps that dedicated,
        // finished model pile's complete prefix unchanged until this process
        // ends; nothing here writes, compacts or truncates it.
        let native = unsafe {
            NativeWemm::from_frozen(
                &source.facts,
                &source.store,
                root,
                pinned,
                &mut aliases,
                |_, _, _, _| {
                    roles += 1;
                    Ok(())
                },
            )
        }?;
        let handles = native.asset_handles();
        let assets = [config, tokenizer, template].map(|bytes| Blob::<RawBytes>::new(bytes.into()));
        anyhow::ensure!(
            [
                handles.config,
                handles.tokenizer_json,
                handles.chat_template
            ] == assets.each_ref().map(Blob::get_handle),
            "bound WeMM asset handles differ from the asset bytes read"
        );
        anyhow::ensure!(
            self::backing(&model)? == backing,
            "the WeMM model file changed while it was bound"
        );
        let identity = WemmIdentity {
            model_collection: collection.handle(),
            model_root: native.model_root(),
            config: handles.config,
            tokenizer_json: handles.tokenizer_json,
            chat_template: handles.chat_template,
            kernel_profile: GB10_NATIVE_BF16_PROFILE,
        };
        let runtime = WemmRuntime::from_native(identity, native, codec)?;
        eprintln!(
            "wemm: bound {roles} tensors from {} in {:.1} s",
            model.display(),
            started.elapsed().as_secs_f64()
        );
        Ok(Self {
            runtime: Rc::new(RefCell::new(runtime)),
            identity,
            model,
            backing,
            _source: source,
            assets,
            descriptor,
        })
    }

    /// The index over `attributes` of `source`: one descriptor from every
    /// machine, registered idempotently. The descriptor names the model
    /// collection and assets, so they are put here too.
    fn register(
        &self,
        store: &mut FacultyStore,
        source: Collection<SimpleArchive>,
        attributes: &[Id],
        reader: &impl StoreRead,
    ) -> Result<(Index, Target)> {
        anyhow::ensure!(
            backing(&self.model)? == self.backing,
            "the WeMM model file changed under the bound session; refusing further model use"
        );
        for blob in &self.assets {
            let _: Inline<inlineencodings::Handle<RawBytes>> = store.put(blob.clone())?;
        }
        let _: CollectionHandle = store.put(self.descriptor.clone())?;
        let mapping = Index::new(
            attributes.iter().copied(),
            self.identity,
            DEFAULT_WINDOW_CAP,
        )?;
        let policy = source.policy(reader)?;
        let target = store.derive_with(source, mapping.clone(), policy)?;
        Ok((mapping, target))
    }

    /// Derive rows for every foundation of `source` that has none, printing
    /// the backlog before and after and each derived foundation on stderr. A
    /// rerun derives only what is still missing; a foundation is published as
    /// soon as it is done, so stopping loses at most the one in progress.
    pub fn index(
        &self,
        store: &mut FacultyStore,
        source: Collection<SimpleArchive>,
        attributes: &[Id],
        signer: &SigningKey,
        runtime: &Arc<tokio::runtime::Runtime>,
        out: &mut Out<'_>,
    ) -> Result<()> {
        let reader = AcquiringReader::new(store.snapshot()?, runtime.clone());
        let (_, target) = self.register(store, source, attributes, &reader)?;
        let name = hex::encode(target.handle().raw);
        let reader = AcquiringReader::new(store.snapshot()?, runtime.clone());
        let backlog = crate::storage::underived(&reader, source, target)?;
        out.line(format!(
            "WeMM index {name}: {backlog} foundation(s) to derive"
        ))?;
        PROGRESS.with(|progress| progress.set((0, backlog)));
        let maintained = with_wemm_runtime(Rc::clone(&self.runtime), || {
            runtime.block_on(store.maintain_with::<Progress>(target, signer))
        })?;
        let after = AcquiringReader::new(store.snapshot()?, runtime.clone());
        let rows = after
            .collection_acquiring(target)?
            .view::<NvFp4CosineIndex<Embedding>>()?
            .len();
        let left = crate::storage::underived(&after, source, target)?;
        out.line(format!(
            "WeMM index {name}: {rows} row(s), {left} foundation(s) not derived yet"
        ))?;
        maintained.map(drop).context("derive the WeMM index")
    }

    /// Score `query` against the rows of `source`'s index as they stand. It
    /// never derives; an index without rows scores nothing.
    pub fn cosines(
        &self,
        store: &mut FacultyStore,
        source: Collection<SimpleArchive>,
        attributes: &[Id],
        runtime: &Arc<tokio::runtime::Runtime>,
        query: Query<'_>,
    ) -> Result<ReconstructedCosines> {
        let reader = AcquiringReader::new(store.snapshot()?, runtime.clone());
        let (mapping, target) = self.register(store, source, attributes, &reader)?;
        let reader = AcquiringReader::new(store.snapshot()?, runtime.clone());
        let index = reader
            .collection_acquiring(target)?
            .view::<NvFp4CosineIndex<Embedding>>()?;
        if index.is_empty() {
            return Ok(ReconstructedCosines::default());
        }
        Ok(with_wemm_runtime(
            Rc::clone(&self.runtime),
            || match query {
                Query::Text(text) => mapping.reconstructed_cosines(&index, text),
                Query::Content(bytes) => mapping.reconstructed_cosines_content(&index, bytes),
            },
        )??)
    }
}

thread_local! {
    /// Foundations derived so far and the backlog an index run started with.
    static PROGRESS: Cell<(usize, usize)> = const { Cell::new((0, 0)) };
}

/// The WeMM mapping, saying on stderr how far a long index run has got. It
/// binds from the same descriptor and maps exactly as [`WemmIndex`] does.
struct Progress(Index);

impl DeriveMapping for Progress {
    type Source = SimpleArchive;
    type Target = NvFp4CosineSet<Embedding>;

    fn fragment(&self) -> Fragment {
        self.0.fragment()
    }

    fn bind(source: &Fragment, target: &Fragment) -> Result<Self, CollectionOperationError> {
        Index::bind(source, target).map(Self)
    }

    fn computable_here(&self) -> bool {
        self.0.computable_here()
    }

    fn map<R: StoreRead>(
        &self,
        source: &Blob<SimpleArchive>,
        reader: &R,
    ) -> Result<Blob<Self::Target>, CollectionOperationError> {
        let started = std::time::Instant::now();
        let image = self.0.map(source, reader)?;
        let (done, backlog) = PROGRESS.with(|progress| {
            let (done, backlog) = progress.get();
            progress.set((done + 1, backlog));
            (done + 1, backlog)
        });
        eprintln!(
            "wemm: derived {done}/{backlog} in {:.1} s",
            started.elapsed().as_secs_f64()
        );
        Ok(image)
    }
}

/// An observed backing file identity: diagnostic evidence, not proof that
/// another process cannot change mapped pages.
#[derive(Clone, Debug, PartialEq, Eq)]
struct BackingIdentity {
    device: u64,
    inode: u64,
    bytes: u64,
}

fn backing(path: &Path) -> Result<BackingIdentity> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    anyhow::ensure!(meta.is_file(), "the WeMM model must be an ordinary file");
    Ok(BackingIdentity {
        device: meta.dev(),
        inode: meta.ino(),
        bytes: meta.len(),
    })
}

/// The model pile is never the pile the indexes are written to, hard links
/// included. A pile that does not exist yet is distinct.
fn distinct_files(model: &BackingIdentity, writable: &Path) -> Result<()> {
    if !writable.exists() {
        return Ok(());
    }
    let other = backing(writable)?;
    anyhow::ensure!(
        (model.device, model.inode) != (other.device, other.inode),
        "the WeMM model pile cannot be the pile its indexes are written to"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_and_writable_hardlinks_are_refused_before_gpu_work() {
        let dir = tempfile::tempdir().unwrap();
        let model = dir.path().join("model.pile");
        let files = dir.path().join("files.pile");
        std::fs::write(&model, b"fixture").unwrap();
        std::fs::hard_link(&model, &files).unwrap();
        assert!(distinct_files(&backing(&model).unwrap(), &files).is_err());
        assert!(distinct_files(&backing(&model).unwrap(), &dir.path().join("new.pile")).is_ok());
    }
}
