//! The one model the faculties search with: WeMM-Embedding-9B, native CUDA
//! BF16 on a GB10, which places text, images and documents in one
//! 4096-dimensional space. Files, Memory and Wiki each derive a collection of
//! NVFP4 rows from their own source collection through
//! [`triblespace_search::semantic_wemm`], keyed by content handle, and join it
//! at the point of use with `find!`.
//!
//! The model lives in the working pile's `models` collection, the exact
//! descriptor `TRIBLESPACE_COLLECTION_MODELS` names, like every other model,
//! and `WEMM_ROOT` names its complete root. Its tokenizer, configuration and
//! chat template are part of that root ([`mary::ingest::RootParts`]) and are
//! read from the same facts as the weights; there is no asset directory.
//!
//! Nobody runs an index command. Only a process with this runtime bound can
//! compute the rows (`DeriveMapping::computable_here`), so the CPU
//! maintenance daemon leaves them alone, and `faculties maintain` binds one
//! [`Session`] and derives what is missing whenever the pile grows. Queries
//! score whatever rows exist and say how many foundations have none yet.
//!
//! Binding aliases the pile's mapped pages straight into CUDA, so those pages
//! must not change until the process ends. A dedicated model pile met that by
//! never being written. The working pile meets it by being append-only: every
//! writer appends after the prefix this binding observed, a compaction writes
//! a new file (renaming it over the path leaves this process's mapping on the
//! old inode, intact, until it exits), and the one operation that shortens a
//! pile in place, `trible pile amputate`, is a manual repair of a torn tail,
//! which lies after any prefix a snapshot could observe. Nobody may run it, or
//! truncate the file any other way, while a WeMM process is bound. The checks
//! below are diagnostics, not proof of that.
//!
//! [`COPY_WEIGHTS_ENV`]` = 1` instead copies every weight into device memory
//! at bind, so forwards never read the mapped pile pages. It costs device
//! memory and bind time, and is off unless chosen. [`probe`] is an optional
//! self-check for the maintenance daemon.

pub mod probe;

use crate::pile_watch::BackingIdentity;
use crate::storage::{AcquiringReader, FacultyStore};
use anyhow::{Context, Result};
use ed25519_dalek::SigningKey;
use mary::models::qwen3_5::input_codec::InputCodec;
use mary::models::qwen3_5::native::{device_identity, Assets, NativeWemm};
use mary::nn::cuda_bf16_alias::{CudaBf16Aliases, WeightResidence};
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use triblespace::core::blob::encodings::simplearchive::SimpleArchive;
use triblespace::core::blob::Blob;
use triblespace::core::collection::{
    Collection, CollectionHandle, CollectionOperationError, CollectionSnapshotExt,
    CollectionStoreExt, DeriveMapping,
};
use triblespace::core::inline::encodings::UnknownInline;
use triblespace::core::repo::StoreRead;
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

/// `1` copies the model's weights into device memory at bind; unset or `0`
/// leaves them read in place from the pile's mapped pages (the default).
/// Read once, when a [`Session`] binds.
pub const COPY_WEIGHTS_ENV: &str = "WEMM_COPY_WEIGHTS";

/// The weight residence [`COPY_WEIGHTS_ENV`]'s value chooses. Any value
/// other than unset, `0` or `1` is refused, so a typo never silently selects
/// the default.
pub fn residence_from_env_value(value: Option<&std::ffi::OsStr>) -> Result<WeightResidence> {
    match value.map(|v| v.to_str()) {
        None => Ok(WeightResidence::PileAliased),
        Some(Some("0")) => Ok(WeightResidence::PileAliased),
        Some(Some("1")) => Ok(WeightResidence::DeviceCopy),
        Some(other) => anyhow::bail!(
            "{COPY_WEIGHTS_ENV} must be 1 (copy weights to device memory), 0 or unset (read them zero-copy from the pile); got {other:?}"
        ),
    }
}

/// The short name of a residence in logs and probe provenance.
pub fn residence_name(residence: WeightResidence) -> &'static str {
    match residence {
        WeightResidence::PileAliased => "pile-aliased",
        WeightResidence::DeviceCopy => "device-copy",
    }
}

/// The line a bind ends with.
pub fn bind_note(
    roles: usize,
    pile: &Path,
    seconds: f64,
    residence: WeightResidence,
    copied_bytes: u64,
) -> String {
    let weights = match residence {
        WeightResidence::DeviceCopy => format!(
            "weights copied to device memory ({:.2} GiB, {COPY_WEIGHTS_ENV}=1)",
            copied_bytes as f64 / (1u64 << 30) as f64
        ),
        WeightResidence::PileAliased => "weights read zero-copy from the pile".to_owned(),
    };
    format!(
        "wemm: bound {roles} tensors from {} in {seconds:.1} s; {weights}",
        pile.display()
    )
}

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
    residence: WeightResidence,
    tokenizer: Option<Id>,
    model: PathBuf,
    backing: BackingIdentity,
    _source: mary::model_collection::ModelPileSnapshot,
}

/// Everything a binding needs that is decided on the CPU: the frozen model
/// collection, the selected root, and the assets read from that root.
pub struct Resolved {
    pub snapshot: mary::model_collection::ModelPileSnapshot,
    pub collection: Collection<SimpleArchive>,
    pub root: Id,
    pub assets: Assets,
}

/// Resolve the model `root` from the `models` collection `handle` of the pile
/// at `pile`, with no device: open the pile read-only, freeze that exact
/// collection, require the root to be stated by it, and read its tokenizer,
/// configuration and chat template from its core (refused when it has none:
/// a member-only root is not a complete WeMM model).
pub fn resolve(pile: &Path, handle: CollectionHandle, root: Id) -> Result<Resolved> {
    let snapshot = crate::model_storage::models_in(pile, handle)?;
    let collection = Collection::<SimpleArchive>::open(snapshot.store(), handle)
        .map_err(|error| anyhow::anyhow!("open the model collection: {error}"))?;
    anyhow::ensure!(
        find!((attribute: Id, value: Inline<UnknownInline>),
            snapshot.facts().pattern::<UnknownInline>(root.to_inline(), attribute, value)
        )
        .next()
        .is_some(),
        "WEMM_ROOT {root:X} is not stated by the model collection of {}",
        pile.display()
    );
    let assets = Assets::from_model_root(snapshot.facts(), snapshot.store(), root)
        .map_err(anyhow::Error::msg)?;
    Ok(Resolved {
        snapshot,
        collection,
        root,
        assets,
    })
}

impl Session {
    /// Bind the model `WEMM_ROOT` names from the `models` collection of the
    /// pile at `pile`, the pile its indexes are written to as well.
    pub fn from_env(pile: &Path) -> Result<Self> {
        let root = std::env::var("WEMM_ROOT")
            .context("WEMM_ROOT is not set; it names the WeMM model root")?;
        let root = Id::from_hex(&root).context("WEMM_ROOT is not a 32-hex-digit id")?;
        let residence = residence_from_env_value(std::env::var_os(COPY_WEIGHTS_ENV).as_deref())?;
        let model = pile.to_path_buf();
        let backing = BackingIdentity::of(&model)?;
        let Resolved {
            snapshot,
            root,
            assets,
            ..
        } = resolve(&model, crate::model_storage::models_handle()?, root)?;
        let codec: InputCodec = assets.codec().clone();
        let tokenizer = mary::selection::model_root_parts(snapshot.facts(), root)
            .ok()
            .and_then(|parts| parts.tokenizer);
        let device = Default::default();
        let observed = device_identity(&device).map_err(anyhow::Error::msg)?;
        anyhow::ensure!(
            observed == ("NVIDIA GB10".into(), 12, 1),
            "WeMM runs on an NVIDIA GB10 (compute 12.1); this is {observed:?}"
        );
        let mut aliases = CudaBf16Aliases::new(device, ROLES)
            .map_err(anyhow::Error::msg)?
            .with_residence(residence);
        let started = std::time::Instant::now();
        let mut roles = 0usize;
        // SAFETY: the pile is append-only and is neither truncated nor
        // rewritten in place while this process runs (module docs); every
        // aliased leaf lies in the prefix `snapshot` observed.
        let native = unsafe {
            NativeWemm::from_frozen(
                snapshot.facts(),
                snapshot.store(),
                root,
                assets,
                &mut aliases,
                |_, _, _, _| {
                    roles += 1;
                    Ok(())
                },
            )
        }?;
        backing.require_still(&model)?;
        let identity = WemmIdentity {
            model_root: native.model_root(),
            kernel_profile: GB10_NATIVE_BF16_PROFILE,
        };
        let runtime = WemmRuntime::from_native(identity, native, codec)?;
        eprintln!(
            "{}",
            bind_note(
                roles,
                &model,
                started.elapsed().as_secs_f64(),
                residence,
                aliases.stats().copied_bytes
            )
        );
        Ok(Self {
            runtime: Rc::new(RefCell::new(runtime)),
            identity,
            residence,
            tokenizer,
            model,
            backing,
            _source: snapshot,
        })
    }

    /// The pile file this session bound, as it observed it.
    pub fn backing(&self) -> &BackingIdentity {
        &self.backing
    }

    /// Where this session's weights live.
    pub fn residence(&self) -> WeightResidence {
        self.residence
    }

    /// Start the [`probe`] self-check if [`probe::PROBE_DIR_ENV`] names a
    /// directory: probe once now, then after every
    /// [`probe::PROBE_EVERY_FOUNDATIONS`] foundations this thread derives.
    /// Returns whether it is on. Only the setup can fail; a later probe that
    /// cannot run is logged and skipped, never an error of the derive.
    pub fn start_probe_from_env(&self) -> Result<bool> {
        let Some(dir) =
            probe::dir_from_env_value(std::env::var_os(probe::PROBE_DIR_ENV).as_deref())
        else {
            return Ok(false);
        };
        let output = self
            .runtime
            .try_borrow_mut()
            .map_err(|_| anyhow::anyhow!("WeMM runtime is busy"))?
            .probe_texts(&probe::PROBE_TEXTS)?;
        let host = std::fs::read_to_string("/proc/sys/kernel/hostname")
            .map(|h| h.trim().to_owned())
            .unwrap_or_else(|_| "unknown".into());
        let identity = probe::Provenance::default()
            .with("model_root", format!("{:X}", self.identity.model_root))
            .with(
                "tokenizer_part",
                self.tokenizer
                    .map_or("unknown".into(), |id| format!("{id:X}")),
            )
            .with(
                "probe_texts_sha256",
                probe::texts_sha256(&probe::PROBE_TEXTS),
            )
            .with("probe_ids_sha256", probe::ids_sha256(&output.ids))
            .with("batch", probe::PROBE_TEXTS.len())
            .with("order", "as listed in faculties::wemm::probe::PROBE_TEXTS")
            .with("weights", residence_name(self.residence))
            .with(
                "executable",
                std::env::current_exe().map_or("unknown".into(), |p| p.display().to_string()),
            )
            .with("executable_sha256", probe::executable_sha256()?)
            .with("faculties_version", crate::GIT_VERSION)
            .with("host", host)
            .with("pid", std::process::id());
        let mut checker = probe::Checker::new(&dir, identity, probe::PROBE_TEXTS.len())?;
        eprintln!(
            "wemm probe: on, every {} derived foundations, results are agreement or disagreement with two baselines (this process's first later-stratum probe and the reference, both taken at forward {} or later), neither an oracle; identity {}: {}",
            probe::PROBE_EVERY_FOUNDATIONS,
            probe::EARLY_FORWARDS,
            checker.identity().digest(),
            checker.identity().line()
        );
        for line in checker.observe(output.forward, output.raw, &now())? {
            eprintln!("{line}");
        }
        PROBE.with(|slot| {
            *slot.borrow_mut() = Some(Probe {
                checker,
                runtime: Rc::clone(&self.runtime),
            })
        });
        Ok(true)
    }

    /// The index over `attributes` of `source`: one descriptor from every
    /// machine, registered idempotently. The descriptor names the model by
    /// its complete root alone, so it carries no blob of its own.
    fn register(
        &self,
        store: &mut FacultyStore,
        source: Collection<SimpleArchive>,
        attributes: &[Id],
        reader: &impl StoreRead,
    ) -> Result<(Index, Target)> {
        self.backing.require_still(&self.model)?;
        let mapping = Index::new(
            attributes.iter().copied(),
            self.identity,
            DEFAULT_WINDOW_CAP,
        )?;
        let policy = source.policy(reader)?;
        let target = store.derive_with(source, mapping.clone(), policy)?;
        Ok((mapping, target))
    }

    /// Derive rows for every foundation of `source` that has none, naming
    /// each derived foundation on stderr. A rerun derives only what is still
    /// missing; a foundation is published as soon as it is done, so stopping
    /// loses at most the one in progress.
    pub fn derive(
        &self,
        store: &mut FacultyStore,
        source: Collection<SimpleArchive>,
        attributes: &[Id],
        signer: &SigningKey,
        runtime: &Arc<tokio::runtime::Runtime>,
    ) -> Result<Derived> {
        let started = std::time::Instant::now();
        let reader = AcquiringReader::new(store.snapshot()?, runtime.clone());
        let (_, target) = self.register(store, source, attributes, &reader)?;
        let reader = AcquiringReader::new(store.snapshot()?, runtime.clone());
        let backlog = crate::storage::underived(&reader, source, target)?;
        PROGRESS.with(|progress| progress.set((0, backlog)));
        let maintained = with_wemm_runtime(Rc::clone(&self.runtime), || {
            runtime.block_on(store.maintain_with::<Progress>(target, signer))
        })?;
        let after = AcquiringReader::new(store.snapshot()?, runtime.clone());
        let derived = Derived {
            index: target.handle(),
            derived: PROGRESS.with(Cell::get).0,
            missing: crate::storage::underived(&after, source, target)?,
            rows: after
                .collection_acquiring(target)?
                .view::<NvFp4CosineIndex<Embedding>>()?
                .len(),
            elapsed: started.elapsed(),
        };
        maintained.with_context(|| format!("derive the WeMM index ({derived})"))?;
        Ok(derived)
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
    ) -> Result<Scores> {
        let reader = AcquiringReader::new(store.snapshot()?, runtime.clone());
        let (mapping, target) = self.register(store, source, attributes, &reader)?;
        let reader = AcquiringReader::new(store.snapshot()?, runtime.clone());
        let missing = crate::storage::underived(&reader, source, target)?;
        let index = reader
            .collection_acquiring(target)?
            .view::<NvFp4CosineIndex<Embedding>>()?;
        if index.is_empty() {
            return Ok(Scores {
                cosines: ReconstructedCosines::default(),
                missing,
            });
        }
        let cosines = with_wemm_runtime(Rc::clone(&self.runtime), || match query {
            Query::Text(text) => mapping.reconstructed_cosines(&index, text),
            Query::Content(bytes) => mapping.reconstructed_cosines_content(&index, bytes),
        })??;
        Ok(Scores { cosines, missing })
    }
}

/// What one derive pass over one index did, and what is left.
#[derive(Clone, Copy, Debug)]
pub struct Derived {
    pub index: CollectionHandle,
    /// Source foundations this pass derived.
    pub derived: usize,
    /// Source foundations that still have no row.
    pub missing: usize,
    /// Rows the index holds after the pass.
    pub rows: usize,
    pub elapsed: std::time::Duration,
}

impl std::fmt::Display for Derived {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "index {}: derived {} foundation(s) in {:.1} s, {} missing, {} row(s)",
            hex::encode(self.index.raw),
            self.derived,
            self.elapsed.as_secs_f64(),
            self.missing,
            self.rows
        )
    }
}

/// A query's cosines over the rows that exist, and how many source
/// foundations have none yet: the answer is partial while that is nonzero.
pub struct Scores {
    pub cosines: ReconstructedCosines,
    pub missing: usize,
}

impl Scores {
    /// The line a semantic answer ends with while its index is partial.
    pub fn partial_note(&self, faculty: &str) -> Option<String> {
        (self.missing > 0).then(|| {
            format!(
                "(partial: {} {faculty} commit(s) have no WeMM rows yet; `faculties maintain` derives them)",
                self.missing
            )
        })
    }

    /// The refusal for an index with no rows at all.
    pub fn require_rows(&self, faculty: &str) -> Result<()> {
        anyhow::ensure!(
            !self.cosines.is_empty(),
            "the {faculty} WeMM index has no rows yet ({} commit(s) to derive); `faculties maintain` derives them on a GB10",
            self.missing
        );
        Ok(())
    }
}

thread_local! {
    /// Foundations derived so far and the backlog a derive pass started with.
    static PROGRESS: Cell<(usize, usize)> = const { Cell::new((0, 0)) };
    /// The running self-check, when [`Session::start_probe_from_env`] started one.
    static PROBE: RefCell<Option<Probe>> = const { RefCell::new(None) };
}

struct Probe {
    checker: probe::Checker,
    runtime: Rc<RefCell<WemmRuntime>>,
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Count one derived foundation and probe when one is due. Never fails: a
/// probe that cannot run is logged and skipped.
fn probe_after_foundation() {
    PROBE.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(probe) = slot.as_mut() else {
            return;
        };
        if !probe.checker.foundation_derived() {
            return;
        }
        let output = match probe.runtime.try_borrow_mut() {
            Ok(mut runtime) => runtime.probe_texts(&probe::PROBE_TEXTS),
            Err(_) => {
                eprintln!("wemm probe: skipped, the runtime is in use");
                return;
            }
        };
        match output {
            Ok(output) => match probe.checker.observe(output.forward, output.raw, &now()) {
                Ok(lines) => lines.iter().for_each(|line| eprintln!("{line}")),
                Err(e) => eprintln!("wemm probe: skipped: {e:#}"),
            },
            Err(e) => eprintln!("wemm probe: skipped: {e}"),
        }
    });
}

/// The WeMM mapping, saying on stderr how far a long derive pass has got. It
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
        // A foundation is one source commit, not one entry: a bulk commit
        // can hold the whole history and take hours, so name it first.
        let (started, backlog) = PROGRESS.with(Cell::get);
        eprintln!(
            "wemm: deriving {}/{backlog}: {} ({} bytes)",
            started + 1,
            hex::encode(source.get_handle().raw),
            source.bytes.len()
        );
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
        probe_after_foundation();
        Ok(image)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weights_stay_in_the_pile_unless_copying_is_chosen() {
        use std::ffi::OsStr;
        assert_eq!(
            residence_from_env_value(None).unwrap(),
            WeightResidence::PileAliased
        );
        assert_eq!(WeightResidence::default(), WeightResidence::PileAliased);
        assert_eq!(
            residence_from_env_value(Some(OsStr::new("0"))).unwrap(),
            WeightResidence::PileAliased
        );
        assert_eq!(
            residence_from_env_value(Some(OsStr::new("1"))).unwrap(),
            WeightResidence::DeviceCopy
        );
        for typo in ["", "yes", "true", " 1", "2"] {
            let error = residence_from_env_value(Some(OsStr::new(typo))).unwrap_err();
            assert!(error.to_string().contains(COPY_WEIGHTS_ENV), "{error}");
        }
    }

    #[test]
    fn the_bind_line_says_where_the_weights_are() {
        let pile = Path::new("/p.pile");
        assert_eq!(
            bind_note(759, pile, 43.04, WeightResidence::DeviceCopy, 18_000_000_000),
            "wemm: bound 759 tensors from /p.pile in 43.0 s; weights copied to device memory (16.76 GiB, WEMM_COPY_WEIGHTS=1)"
        );
        assert_eq!(
            bind_note(759, pile, 16.0, WeightResidence::PileAliased, 0),
            "wemm: bound 759 tensors from /p.pile in 16.0 s; weights read zero-copy from the pile"
        );
    }

    /// The CPU half of a binding against a real pile: the complete root
    /// resolves from `WEMM_RESOLVE_PILE`'s collection `WEMM_RESOLVE_MODELS`
    /// (64 hex digits), its assets validate, and the rebuilt tokenizer frames
    /// text exactly like the checkpoint's `tokenizer.json` at
    /// `WEMM_RESOLVE_TOKENIZER_JSON` with the template at
    /// `WEMM_RESOLVE_TEMPLATE`. No device is touched.
    #[test]
    #[ignore = "requires a pile holding the complete WeMM root"]
    fn the_complete_root_resolves_without_a_device() {
        let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name}"));
        let handle =
            mary::model_collection::parse_collection_handle(&var("WEMM_RESOLVE_MODELS")).unwrap();
        let root = Id::from_hex(&var("WEMM_RESOLVE_ROOT")).unwrap();
        let started = std::time::Instant::now();
        let resolved = resolve(Path::new(&var("WEMM_RESOLVE_PILE")), handle, root).unwrap();
        eprintln!(
            "resolved root {root:X} from collection {} ({} facts) in {:.1} s",
            hex::encode(resolved.collection.handle().raw),
            resolved.snapshot.facts().len(),
            started.elapsed().as_secs_f64()
        );
        let reference = InputCodec::from_assets(
            &std::fs::read(var("WEMM_RESOLVE_TOKENIZER_JSON")).unwrap(),
            &std::fs::read(var("WEMM_RESOLVE_TEMPLATE")).unwrap(),
        )
        .unwrap();
        let codec = resolved.assets.codec();
        let texts = [
            "hello",
            "a red crab with claws",
            "The Rust borrow checker enforces ownership: one mutable reference or many shared ones.",
            "  naïve façade, 漢字, emoji 🦀, tabs\tand\nnewlines  ",
            "<think>x</think> <tool_call>y</tool_call>",
        ];
        for text in texts {
            assert_eq!(
                codec.text(text).unwrap().as_slice(),
                reference.text(text).unwrap().as_slice(),
                "{text:?}"
            );
        }
        assert_eq!(
            codec.image().unwrap().as_slice(),
            reference.image().unwrap().as_slice()
        );
        assert!(codec.text("<embedding>").is_err());
        let ids = codec.text("hello").unwrap();
        eprintln!(
            "{} texts and the image placeholder framed identically; \"hello\" -> {:?}",
            texts.len(),
            ids.as_slice()
        );
    }

    /// The real model on a scratch pile: five memories (one a wordless image
    /// of a red crab), three wiki fragments and two files, indexed through one
    /// bound session, then found again by text queries. Needs a GB10 under
    /// the gb10 lock, `WEMM_MODEL_PILE` (a pile whose `models` collection
    /// holds the complete root; its contents are copied into the scratch pile
    /// first), `TRIBLESPACE_COLLECTION_MODELS`, `WEMM_ROOT`, and
    /// `WEMM_GATE_PNG` naming a PNG of a red crab.
    #[test]
    #[ignore = "requires a GB10, a pile holding the complete WeMM root and WEMM_GATE_PNG"]
    fn memory_wiki_and_files_answer_text_queries_on_a_scratch_pile() {
        use crate::memory::operations::parse_time_range;
        use crate::out::{Out, Part};

        let png = std::fs::read(std::env::var("WEMM_GATE_PNG").unwrap()).unwrap();
        let model_pile = std::env::var("WEMM_MODEL_PILE").unwrap();
        let models = std::env::var("TRIBLESPACE_COLLECTION_MODELS").unwrap();
        crate::test_support::clear_ambient_environment();
        let dir = tempfile::tempdir().unwrap();
        let pile = dir.path().join("scratch.pile");
        // The model lives in the pile its indexes are written to; a copy, so
        // the test's writes never reach the model pile it was given.
        std::fs::copy(&model_pile, &pile).unwrap();
        std::env::set_var("TRIBLESPACE_COLLECTION_MODELS", models);
        crate::storage::initialize_signer(&pile, None).unwrap();
        let storage = crate::storage::Storage::shared(pile.clone(), None);
        let memory = crate::memory::Memory::with_storage(storage.clone());
        let wiki = crate::wiki::Wiki::with_storage(storage.clone());
        let files = crate::files::Files::with_storage(storage.clone());

        let day = |n: u32| {
            parse_time_range(&format!("2026-01-0{n}T08:00:00..2026-01-0{n}T09:00:00")).unwrap()
        };
        let texts = [
            "We repaired the bicycle chain in the garage and oiled the rusty gears.",
            "The sourdough starter finally rose, and we baked two loaves of rye bread.",
            "A thunderstorm knocked out the power, so we read by candlelight all evening.",
            "We tuned the guitar and practised the chord progression for the new song.",
        ];
        let chunks: Vec<Id> = texts
            .iter()
            .zip(1..)
            .map(|(text, n)| memory.create(text, Some(day(n)), &[]).unwrap().id)
            .collect();
        let crab = memory.image(&png, day(5)).unwrap().id;
        let revisions: Vec<Id> = [
            ("Borrowing", "= Borrowing\nThe Rust borrow checker enforces ownership: one mutable reference or many shared ones, never both."),
            ("Tomatoes", "= Tomatoes\nTomato seedlings want full sun, deep watering and a stake once they reach knee height."),
            ("Tides", "= Tides\nThe moon's gravity raises two bulges of ocean water, giving most coasts two high tides a day."),
        ]
        .into_iter()
        .map(|(title, content)| wiki.create(title, content, &[], false).unwrap())
        .collect();
        // The tides revision is superseded: its rows exist, but it is not current.
        let tides = revisions[2];
        let volcanoes = wiki
            .edit(
                &format!("{tides:x}"),
                Some("= Tides\nVolcanoes erupt molten rock and ash when magma pressure breaks the crust."),
                None,
                &[],
                false,
            )
            .unwrap();
        files
            .add_bytes(
                anybytes::Bytes::from_source(png.clone()),
                "crab.png",
                "image/png",
                &[],
            )
            .unwrap();
        files
            .add_bytes(
                anybytes::Bytes::from_source(
                    b"Quarterly tax return: list deductions, attach receipts, file before April."
                        .to_vec(),
                ),
                "taxes.txt",
                "text/plain",
                &[],
            )
            .unwrap();

        let run = |operation: &mut dyn FnMut(&mut Out<'_>) -> Result<()>| {
            let mut text = String::new();
            let mut emit = |part: Part| {
                if let Part::Text { text: line } = part {
                    text.push_str(&line);
                }
                Ok(())
            };
            operation(&mut Out::new(&mut emit)).unwrap();
            text
        };
        let session = Session::from_env(&pile).unwrap();
        let started = std::time::Instant::now();
        for derived in [
            memory.derive_wemm(&session).unwrap(),
            wiki.derive_wemm(&session).unwrap(),
            files.derive_wemm(&session).unwrap(),
        ] {
            eprintln!("{derived}");
            assert_eq!(derived.missing, 0, "{derived}");
        }
        eprintln!("indexed in {:.1} s", started.elapsed().as_secs_f64());

        let first = |report: String| report.lines().next().unwrap_or_default().to_owned();
        let bread = run(&mut |out| memory.similar_with(&session, "baking bread at home", out));
        eprintln!("{bread}");
        assert!(first(bread).contains(&format!("{:x}", chunks[1])));
        let crustacean =
            run(&mut |out| memory.similar_with(&session, "a red crab with claws", out));
        eprintln!("{crustacean}");
        assert!(
            first(crustacean).contains(&format!("{crab:x}")),
            "an image chunk answers text"
        );
        let rust = wiki
            .similar_with(&session, "how references and mutation are checked")
            .unwrap();
        eprintln!("{rust}");
        assert!(first(rust).contains(&format!("{:x}", revisions[0])));
        let ocean = wiki
            .similar_with(&session, "the moon pulls the ocean into high tides")
            .unwrap();
        eprintln!("{ocean}");
        assert!(
            !ocean.contains(&format!("{tides:x}")),
            "a superseded revision is not answered"
        );
        let lava = wiki
            .similar_with(&session, "lava from an erupting volcano")
            .unwrap();
        eprintln!("{lava}");
        assert!(first(lava).contains(&format!("{volcanoes:x}")));
        let picture = run(&mut |out| {
            files.similar_with(
                &session,
                &crate::files::operations::SimilarityOptions {
                    id: None,
                    text: Some("a red crab with claws"),
                    floor: None,
                    limit: 2,
                    tags: &[],
                },
                out,
            )
        });
        eprintln!("{picture}");
        assert!(picture
            .lines()
            .nth(1)
            .unwrap_or_default()
            .contains("crab.png"));
        storage.close().unwrap();
    }
}
