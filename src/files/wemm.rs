//! Explicit native WeMM Files operations. One resident model supplies text and
//! images. Historical Nomic commands remain separate; no fallback is implicit.
//!
//! The CLI's selected, dedicated model artifact is under operator immutable
//! prefix custody through CUDA runtime teardown. A read-only descriptor and
//! these stat checks do not establish that unsafe premise against outsiders.
//! Library callers must explicitly establish it when binding a source.

use super::*;
use mary::models::qwen3_5::native::{Assets, NativeWemm};
use mary::nn::cuda_bf16_alias::{AliasStats, CudaBf16Aliases};
use std::{cell::RefCell, rc::Rc};
use triblespace::core::collection::CollectionHandle;
use triblespace::core::inline::encodings::UnknownInline;
use triblespace::core::repo::BlobStorePut;
use triblespace_search::{
    nvfp4::{NvFp4CosineIndex, NvFp4CosineSet},
    schemas::Embedding,
    semantic_wemm::{
        GB10_NATIVE_BF16_PROFILE, WemmIdentity, WemmIndex, WemmRuntime, with_wemm_runtime,
    },
};

type Index = WemmIndex<Embedding>;
type Target = Collection<NvFp4CosineSet<Embedding>>;

/// Explicit artifact selection, never a model discovery catalogue. The root is
/// opaque and must be stated in the supplied model collection observation.
#[derive(Clone, Debug)]
pub struct ModelOptions {
    pub pile: PathBuf,
    pub collection: CollectionHandle,
    pub root: Id,
    pub assets: PathBuf,
}

/// An observed backing file identity, diagnostic evidence rather than proof
/// that another process cannot modify mapped pages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackingIdentity {
    pub device: u64,
    pub inode: u64,
    pub bytes: u64,
}

#[cfg(unix)]
fn backing(path: &Path) -> Result<BackingIdentity> {
    use std::os::unix::fs::MetadataExt;
    let meta = fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    anyhow::ensure!(meta.is_file(), "model backing must be an ordinary file");
    Ok(BackingIdentity {
        device: meta.dev(),
        inode: meta.ino(),
        bytes: meta.len(),
    })
}

#[cfg(not(unix))]
fn backing(_path: &Path) -> Result<BackingIdentity> {
    bail!("this native WeMM artifact-custody boundary requires Unix file identity")
}

fn distinct_files(model: &BackingIdentity, files: &Path) -> Result<()> {
    match backing(files) {
        Ok(other) => anyhow::ensure!(
            (model.device, model.inode) != (other.device, other.inode),
            "WeMM model backing cannot be the writable Files pile (including hard links)"
        ),
        Err(error) if !files.exists() => {
            // The Files store may be deliberately new. Other stat failures
            // must not be disguised as absence.
            if !error.chain().any(|cause| {
                cause
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
            }) {
                return Err(error);
            }
        }
        Err(error) => return Err(error),
    }
    Ok(())
}

/// Validated assets and a genuine frozen model-pile observation; opening this
/// source performs no GPU initialization and creates no writer.
pub struct ModelSource {
    options: ModelOptions,
    backing: BackingIdentity,
    source: mary::persist::ModelPileSource,
    assets: Assets,
    asset_blobs: [Blob<blobencodings::RawBytes>; 3],
    descriptor: Blob<SimpleArchive>,
}

impl ModelSource {
    pub fn open(options: ModelOptions, files_pile: &Path) -> Result<Self> {
        // Pin/refusal validation precedes the device or alias session.
        let config = fs::read(options.assets.join("config.json"))?;
        let tokenizer = fs::read(options.assets.join("tokenizer.json"))?;
        let template = fs::read(options.assets.join("chat_template.jinja"))?;
        let assets =
            Assets::from_bytes(&config, &tokenizer, &template).map_err(anyhow::Error::msg)?;
        let asset_blobs = [config, tokenizer, template]
            .map(|bytes| Blob::<blobencodings::RawBytes>::new(bytes.into()));
        let identity = backing(&options.pile)?;
        distinct_files(&identity, files_pile)?;
        let source = mary::persist::read_model_pile_read_only(&options.pile)?;
        anyhow::ensure!(
            source.collections.len() == 1,
            "this bounded WeMM loader supports one contributing model collection; mixed graph/bundle artifacts require explicit per-collection selection"
        );
        let collection = Collection::<SimpleArchive>::open(&source.store, options.collection)?;
        anyhow::ensure!(
            source
                .collections
                .iter()
                .any(|item| item.collection == collection),
            "selected collection is not a contributing native model collection"
        );
        let selected =
            mary::model_collection::snapshot_model_collection_for(&source.store, collection)?;
        let root = options.root.to_inline();
        anyhow::ensure!(
            find!((attribute: Id, value: Inline<UnknownInline>),
                selected.facts().pattern::<UnknownInline>(root, attribute, value)
            )
            .next()
            .is_some(),
            "selected root is not stated by the selected model collection"
        );
        let descriptor = source.store.get(options.collection)?;
        anyhow::ensure!(
            backing(&options.pile)? == identity,
            "model file identity changed while selecting its frozen prefix"
        );
        Ok(Self {
            options,
            backing: identity,
            source,
            assets,
            asset_blobs,
            descriptor,
        })
    }

    /// Bind through the caller's bounded, reused session. On an error the
    /// caller MUST keep that same binder; registrations are not undone.
    ///
    /// # Safety
    /// The complete genuine pile prefix (including preceding partial pages)
    /// must remain unchanged and untruncated until CUDA runtime teardown.
    /// No external compactor/writer may violate it. Read-only open/stat checks
    /// are diagnostics, not proof. Dropping this source/session is not teardown.
    pub unsafe fn bind(self, aliases: &mut CudaBf16Aliases) -> Result<Session> {
        let mut role_count = 0usize;
        let mut selection = blake3::Hasher::new();
        // SAFETY: exactly this method's documented caller obligation.
        let native = unsafe {
            NativeWemm::from_frozen(
                &self.source.facts,
                &self.source.store,
                self.options.root,
                self.assets,
                aliases,
                |role, handle, shape, _payload| {
                    selection.update(&(role.len() as u64).to_le_bytes());
                    selection.update(role.as_bytes());
                    selection.update(&handle);
                    selection.update(&(shape.len() as u64).to_le_bytes());
                    for dim in shape {
                        selection.update(&dim.to_le_bytes());
                    }
                    role_count += 1;
                    Ok(())
                },
            )
        }?;
        let assets = native.asset_handles();
        anyhow::ensure!(
            [assets.config, assets.tokenizer_json, assets.chat_template]
                == self.asset_blobs.each_ref().map(Blob::get_handle),
            "bound WeMM asset handles differ from selected bytes"
        );
        let identity = WemmIdentity {
            model_collection: self.options.collection,
            model_root: native.model_root(),
            config: assets.config,
            tokenizer_json: assets.tokenizer_json,
            chat_template: assets.chat_template,
            kernel_profile: GB10_NATIVE_BF16_PROFILE,
        };
        let runtime = WemmRuntime::from_native(identity, native)?;
        Ok(Session {
            runtime: Rc::new(RefCell::new(runtime)),
            identity,
            backing: self.backing,
            model_path: self.options.pile,
            _source: self.source,
            asset_blobs: self.asset_blobs,
            descriptor: self.descriptor,
            role_count,
            selection_blake3: selection.finalize().to_hex().to_string(),
            aliases_after_load: aliases.stats(),
        })
    }
}

/// Process/thread-local operational resources, not a semantic catalogue.
/// This Rc session is deliberately !Send: the lease encloses synchronous
/// queries and direct current-thread block_on. Never spawn/return a future
/// outside it. Repeated operations reuse the same bound model.
pub struct Session {
    runtime: Rc<RefCell<WemmRuntime>>,
    identity: WemmIdentity,
    backing: BackingIdentity,
    model_path: PathBuf,
    _source: mary::persist::ModelPileSource,
    asset_blobs: [Blob<blobencodings::RawBytes>; 3],
    descriptor: Blob<SimpleArchive>,
    pub role_count: usize,
    pub selection_blake3: String,
    pub aliases_after_load: AliasStats,
}

impl Session {
    pub fn backing_identity(&self) -> &BackingIdentity {
        &self.backing
    }
    pub fn identity(&self) -> WemmIdentity {
        self.identity
    }

    fn check_destination(&self, path: &Path) -> Result<()> {
        anyhow::ensure!(
            backing(&self.model_path)? == self.backing,
            "selected model backing changed; refusing further model use"
        );
        distinct_files(&self.backing, path)
    }

    fn register(
        &self,
        store: &mut FacultyStore,
        source: Collection<SimpleArchive>,
        reader: &impl triblespace::core::repo::StoreRead,
    ) -> Result<(Index, Target)> {
        // Strong descriptor references remain resident without copying weights
        // or pretending the model collection's records were replicated here.
        for blob in &self.asset_blobs {
            let _: FileHandle = store.put(blob.clone())?;
        }
        let _: CollectionHandle = store.put(self.descriptor.clone())?;
        let mapping = Index::new([file::content.id()], self.identity)?;
        let policy = source.policy(reader)?;
        let target = store.derive_with(source, mapping.clone(), policy)?;
        Ok((mapping, target))
    }
}

pub struct IndexReport {
    pub collection: Target,
    /// Physical rows in the retained cover, including multiple admitted rows
    /// for one content. Scoring binds each content once at the maximum.
    pub rows: usize,
    pub underived_foundations: usize,
}

pub enum Query<'a> {
    Text(&'a str),
    File(&'a str),
}
pub struct QueryOptions<'a> {
    pub query: Query<'a>,
    /// No calibrated universal relevance threshold. None ranks all finite
    /// reconstructed scores; an explicit floor is in [-1,1].
    pub floor: Option<f64>,
    pub limit: usize,
    pub tags: &'a [String],
}
#[derive(Debug)]
pub struct Hit {
    pub content: FileHandle,
    pub entity: Id,
    pub name: String,
    /// Approximate reconstructed NVFP4 cosine, not an exact source rerank.
    pub cosine: f64,
}

impl Files {
    /// Explicit WeMM maintenance; refuses unsupported content as a failed
    /// foundation. It never redirects to historical Nomic or publishes a
    /// successful empty leaf for a refused selected content value.
    pub fn index_wemm(&self, session: &mut Session) -> Result<IndexReport> {
        session.check_destination(self.storage.path())?;
        with_wemm_runtime(Rc::clone(&session.runtime), || {
            with_files_store(&self.storage, |store, source, signer, runtime| {
                let frozen = AcquiringReader::new(store.snapshot()?, runtime.clone());
                let (_, target) = session.register(store, source, &frozen)?;
                // Direct block_on: no spawned/migrating mapping future escapes
                // the owning thread's runtime lease.
                let after = runtime.block_on(store.maintain_with::<Index>(target, signer))?;
                let after = AcquiringReader::new(after, runtime.clone());
                let index = after
                    .collection_acquiring(target)?
                    .view::<NvFp4CosineIndex<Embedding>>()?;
                Ok(IndexReport {
                    collection: target,
                    rows: index.len(),
                    underived_foundations: crate::storage::underived(&after, source, target)?,
                })
            })
        })?
    }

    /// Rank one unified WeMM space through the native GPU query/scorer and an
    /// ordinary relational Files join. Query does not maintain/index backlog.
    pub fn similar_wemm(
        &self,
        session: &mut Session,
        options: &QueryOptions<'_>,
    ) -> Result<Vec<Hit>> {
        if let Some(floor) = options.floor {
            anyhow::ensure!(
                floor.is_finite() && (-1.0..=1.0).contains(&floor),
                "WeMM floor must be a finite cosine in [-1,1]"
            );
        }
        session.check_destination(self.storage.path())?;
        with_wemm_runtime(Rc::clone(&session.runtime), || {
            with_files_view(
                &self.storage,
                |store, source, _, facts, snapshot, runtime| {
                    let reader = AcquiringReader::new(snapshot.clone(), runtime.clone());
                    let (mapping, target) = session.register(store, source, &reader)?;
                    let observed = AcquiringReader::new(store.snapshot()?, runtime.clone());
                    let index = observed
                        .collection_acquiring(target)?
                        .view::<NvFp4CosineIndex<Embedding>>()?;
                    anyhow::ensure!(
                        !index.is_empty(),
                        "native WeMM index has no rows; run files index --wemm"
                    );
                    let mut exclude = None;
                    let cosines = match options.query {
                        Query::Text(text) => mapping.reconstructed_cosines(&index, text)?,
                        Query::File(selector) => {
                            let entity = file_capability::resolve_selector(facts, selector)?;
                            let content = content_handle_of(facts, entity)
                                .context("query file has no content")?;
                            let bytes: anybytes::Bytes = reader.get(content)?;
                            exclude = Some(content);
                            mapping.reconstructed_cosines_content(&index, bytes.as_ref())?
                        }
                    };
                    let tags: Vec<Inline<inlineencodings::ShortString>> = options
                        .tags
                        .iter()
                        .map(|tag| {
                            tag.as_str()
                                .try_to_inline()
                                .map_err(|_| anyhow::anyhow!("invalid Files tag {tag:?}"))
                        })
                        .collect::<Result<_>>()?;
                    use triblespace::core::query::Constraint;
                    let tag_attr: Inline<inlineencodings::GenId> = file::tag.id().to_inline();
                    // Scratch answer for this query only. One content can have
                    // several Files holders; keep them visible, not a shadow index.
                    let mut hits = Vec::new();
                    for (content, entity) in find!((content: FileHandle, entity: Id), and!(
                        cosines.similar_to::<inlineencodings::Handle<blobencodings::RawBytes>>(
                            content, options.floor.unwrap_or(f64::NEG_INFINITY)),
                        pattern!(facts, [{ ?entity @ file::content: ?content }]),
                        IntersectionConstraint::new(tags.iter().map(|tag| {
                            Box::new(facts.pattern(entity, tag_attr, *tag)) as Box<dyn Constraint + Send + Sync>
                        }).collect()),
                    )) {
                        if Some(content) == exclude {
                            continue;
                        }
                        let cosine = cosines.cosine(&content).context("query score vanished")?;
                        hits.push(Hit {
                            content,
                            entity,
                            cosine,
                            name: read_name(facts, &reader, entity)?.unwrap_or_else(|| "?".into()),
                        });
                    }
                    hits.sort_by(|a, b| {
                        b.cosine
                            .total_cmp(&a.cosine)
                            .then(a.content.cmp(&b.content))
                            .then(a.entity.cmp(&b.entity))
                    });
                    hits.truncate(options.limit);
                    Ok(hits)
                },
            )
        })?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_and_files_hardlinks_are_refused_before_gpu_work() {
        let dir = tempfile::tempdir().unwrap();
        let model = dir.path().join("model.pile");
        let files = dir.path().join("files.pile");
        fs::write(&model, b"fixture").unwrap();
        fs::hard_link(&model, &files).unwrap();
        assert!(distinct_files(&backing(&model).unwrap(), &files).is_err());
        assert!(distinct_files(&backing(&model).unwrap(), &dir.path().join("new.pile")).is_ok());
    }

    /// Real native model -> GPU encoder -> persisted DERIVE -> Files find!
    /// query. The CPU quantizer below is only the already-established row
    /// recipe oracle over retained native BF16 bits, not model inference.
    #[test]
    #[ignore = "requires reserved GB10, immutable model custody and retained fixture evidence"]
    fn native_files_derives_text_and_image_and_refuses_partial_long_foundation() {
        use std::collections::BTreeSet;
        use triblespace::core::collection::{
            CollectionRead, CollectionRecord, CollectionRecordSelector, SourceLocator,
        };
        let path = |name| PathBuf::from(std::env::var(name).expect(name));
        let output = path("WEMM_FILES_SCRATCH");
        fs::create_dir(&output).expect("fresh absent scratch directory");
        let pile = output.join("files.pile");
        fs::File::create_new(&pile).unwrap();
        crate::storage::initialize_signer(&pile, None).unwrap();
        let storage = Storage::shared(pile.clone(), None);
        let files = Files::with_storage(storage.clone());
        let fixture: serde_json::Value =
            serde_json::from_slice(&fs::read(path("WEMM_FILES_FIXTURE")).unwrap()).unwrap();
        let baseline: serde_json::Value =
            serde_json::from_slice(&fs::read(path("WEMM_FILES_NATIVE")).unwrap()).unwrap();
        assert_eq!(baseline["engine"], "native-CUDA-BF16");
        let item = |id: &str| {
            fixture["items"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["id"] == id)
                .unwrap()
        };
        let records = |target: Target| -> Vec<CollectionRecord> {
            storage
                .with_store(|store, _, _| {
                    Ok(store.snapshot()?.select_records(&BTreeSet::from([
                        CollectionRecordSelector::Collection(target.handle()),
                    ]))?)
                })
                .unwrap()
        };
        // Each source foundation is a real Files fragment, one content each.
        let mut published = Vec::new();
        for id in ["ferris-short", "ferris-image", "bloom-short"] {
            let selected = item(id);
            let (bytes, mime) = if selected["modality"] == "image" {
                assert!(selected.get("crop").is_none_or(serde_json::Value::is_null));
                (
                    fs::read(selected["source_path"].as_str().unwrap()).unwrap(),
                    "image/png",
                )
            } else {
                (
                    selected["text"].as_str().unwrap().as_bytes().to_vec(),
                    "text/plain",
                )
            };
            let fragment = file_capability::stage(bytes, id, mime).unwrap();
            let entity = fragment.root().unwrap();
            let content = content_handle_of(fragment.facts(), entity).unwrap();
            let commit = with_files_store(&storage, |store, source, signer, _| {
                Ok(store.commit(source, signer, fragment)?)
            })
            .unwrap();
            published.push((id, entity, content, SourceLocator::of(commit.data().raw)));
        }
        let model_pile = path("WEMM_FILES_MODEL");
        let model = mary::persist::read_model_pile_read_only(&model_pile).unwrap();
        assert_eq!(model.collections.len(), 1, "bounded dedicated artifact");
        let collection = model.collections[0].collection.handle();
        drop(model);
        let model_source = ModelSource::open(
            ModelOptions {
                pile: model_pile,
                collection,
                root: Id::from_hex(&std::env::var("WEMM_FILES_ROOT").unwrap()).unwrap(),
                assets: path("WEMM_CHECKPOINT_DIR"),
            },
            &pile,
        )
        .unwrap();
        let device = Default::default();
        assert_eq!(
            mary::models::qwen3_5::native::device_identity(&device).unwrap(),
            ("NVIDIA GB10".into(), 12, 1)
        );
        let mut aliases = CudaBf16Aliases::new(device, 759).unwrap();
        let started = std::time::Instant::now();
        // SAFETY: runner pins the dedicated finished model artifact and keeps
        // its complete mapped prefix immutable for this process lifetime.
        let mut session = unsafe { model_source.bind(&mut aliases) }.unwrap();
        eprintln!(
            "Files WeMM binding_ms={} roles={} selection={}",
            started.elapsed().as_millis(),
            session.role_count,
            session.selection_blake3
        );
        assert_eq!(session.role_count, 759);
        assert_eq!(session.aliases_after_load.registrations, 759);
        let bound = aliases.stats();
        let started = std::time::Instant::now();
        let report = files.index_wemm(&mut session).unwrap();
        assert_eq!(report.rows, 3);
        assert_eq!(report.underived_foundations, 0);
        let leaves: Vec<_> = records(report.collection)
            .into_iter()
            .filter_map(|r| {
                if let CollectionRecord::Derive(leaf) = r {
                    Some(leaf)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(leaves.len(), 3);
        for (id, _, content, locator) in &published {
            let expected = baseline["embeddings"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["id"] == *id && row["pass"] == "forward")
                .unwrap();
            let values: Vec<f32> = expected["bits"]
                .as_array()
                .unwrap()
                .iter()
                .map(|word| f32::from_bits((word.as_u64().unwrap() as u32) << 16))
                .collect();
            let row = mary::nn::nvfp4_cosine::QuantizedRow::quantize(&values, 4096).unwrap();
            let expected =
                NvFp4CosineSet::<Embedding>::from_quantized_rows_4096([(content.raw, row)])
                    .unwrap();
            let leaf = leaves.iter().find(|leaf| leaf.input() == *locator).unwrap();
            assert_eq!(
                leaf.output().raw,
                expected.get_handle().raw,
                "exact encoded native row {id}"
            );
        }
        eprintln!(
            "Files WeMM index_ms={} DERIVEs={}",
            started.elapsed().as_millis(),
            leaves.len()
        );
        let hits = files
            .similar_wemm(
                &mut session,
                &QueryOptions {
                    query: Query::Text(item("ferris-query")["text"].as_str().unwrap()),
                    floor: None,
                    limit: 3,
                    tags: &[],
                },
            )
            .unwrap();
        assert_eq!(hits.len(), 3);
        let score = |id: &str| hits.iter().find(|hit| hit.name == id).unwrap().cosine;
        assert!(score("ferris-image") > score("bloom-short"));
        assert!(score("ferris-short") > score("bloom-short"));
        for hit in &hits {
            eprintln!("Files WeMM hit {} {}", hit.name, hit.cosine);
        }
        let image_entity = published.iter().find(|p| p.0 == "ferris-image").unwrap().1;
        let selector = format!("{image_entity:x}");
        let reciprocal = files
            .similar_wemm(
                &mut session,
                &QueryOptions {
                    query: Query::File(&selector),
                    floor: None,
                    limit: 3,
                    tags: &[],
                },
            )
            .unwrap();
        assert_eq!(reciprocal.len(), 2, "query content itself excluded");
        assert_eq!(reciprocal[0].name, "ferris-short");
        // One archive contains BOTH a valid selected value and unsupported long
        // text. It may not publish a successful partial or empty leaf.
        let mut refused = file_capability::stage(
            "x ".repeat(1024).into_bytes(),
            "unsupported-long",
            "text/plain",
        )
        .unwrap();
        refused += file_capability::stage(
            item("ferris-short")["text"]
                .as_str()
                .unwrap()
                .as_bytes()
                .to_vec(),
            "valid-in-refused-archive",
            "text/plain",
        )
        .unwrap();
        let refused_commit = with_files_store(&storage, |store, source, signer, _| {
            Ok(store.commit(source, signer, refused)?)
        })
        .unwrap();
        let error = files
            .index_wemm(&mut session)
            .err()
            .expect("unsupported length must fail");
        assert!(format!("{error:#}").contains("256"), "{error:#}");
        let refused_locator = SourceLocator::of(refused_commit.data().raw);
        assert!(
            !records(report.collection)
                .iter()
                .any(|record| matches!(record,
            CollectionRecord::Derive(leaf) if leaf.input() == refused_locator))
        );
        assert_eq!(aliases.stats(), bound, "operations never rebind weights");
        // Same selected runtime and data can still answer after the refusal.
        assert_eq!(
            files
                .similar_wemm(
                    &mut session,
                    &QueryOptions {
                        query: Query::Text(item("ferris-query")["text"].as_str().unwrap()),
                        floor: None,
                        limit: 10,
                        tags: &[],
                    }
                )
                .unwrap()
                .len(),
            4
        ); // the valid content now has a second holder
        storage.close().unwrap();
        println!(
            "Files native WeMM scratch PASS: 3 native DERIVEs, exact rows, text/image find!, mixed-long archive refused, no rebinding"
        );
    }
}
