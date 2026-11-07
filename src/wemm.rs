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

use crate::out::Out;
use crate::storage::{AcquiringReader, FacultyStore};
use anyhow::{Context, Result};
use ed25519_dalek::SigningKey;
use mary::models::qwen3_5::input_codec::InputCodec;
use mary::models::qwen3_5::native::{device_identity, Assets, NativeWemm};
use mary::nn::cuda_bf16_alias::CudaBf16Aliases;
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
        let model = pile.to_path_buf();
        let backing = backing(&model)?;
        let Resolved {
            snapshot,
            root,
            assets,
            ..
        } = resolve(&model, crate::model_storage::models_handle()?, root)?;
        let codec: InputCodec = assets.codec().clone();
        let device = Default::default();
        let observed = device_identity(&device).map_err(anyhow::Error::msg)?;
        anyhow::ensure!(
            observed == ("NVIDIA GB10".into(), 12, 1),
            "WeMM runs on an NVIDIA GB10 (compute 12.1); this is {observed:?}"
        );
        let mut aliases = CudaBf16Aliases::new(device, ROLES).map_err(anyhow::Error::msg)?;
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
            "wemm: bound {roles} tensors from {} in {:.1} s",
            model.display(),
            started.elapsed().as_secs_f64()
        );
        Ok(Self {
            runtime: Rc::new(RefCell::new(runtime)),
            identity,
            model,
            backing,
            _source: snapshot,
        })
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
    anyhow::ensure!(
        meta.is_file(),
        "the WeMM model pile must be an ordinary file"
    );
    Ok(BackingIdentity {
        device: meta.dev(),
        inode: meta.ino(),
        bytes: meta.len(),
    })
}

impl BackingIdentity {
    /// The pile at `path` is still this file, at least as long as it was:
    /// appends are expected (the working pile grows), a replaced or shortened
    /// file is not.
    fn require_still(&self, path: &Path) -> Result<()> {
        let now = backing(path)?;
        anyhow::ensure!(
            (now.device, now.inode) == (self.device, self.inode),
            "the WeMM model pile {} was replaced under the bound session; refusing further model use",
            path.display()
        );
        anyhow::ensure!(
            now.bytes >= self.bytes,
            "the WeMM model pile {} shrank under the bound session ({} -> {} bytes); refusing further model use",
            path.display(),
            self.bytes,
            now.bytes
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_grown_pile_is_still_bound_and_a_replaced_or_shortened_one_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let pile = dir.path().join("self.pile");
        std::fs::write(&pile, b"prefix").unwrap();
        let bound = backing(&pile).unwrap();
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&pile)
            .unwrap()
            .write_all(b" appended")
            .unwrap();
        bound.require_still(&pile).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&pile)
            .unwrap()
            .set_len(3)
            .unwrap();
        assert!(bound.require_still(&pile).is_err(), "shortened");
        let other = dir.path().join("compacted.pile");
        std::fs::write(&other, b"prefix appended and more").unwrap();
        std::fs::rename(&other, &pile).unwrap();
        assert!(bound.require_still(&pile).is_err(), "replaced");
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
        use crate::out::Part;

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
        for report in [
            run(&mut |out| memory.index_with(&session, out)),
            run(&mut |out| wiki.index_with(&session, out)),
            run(&mut |out| files.index_with(&session, out)),
        ] {
            eprintln!("{report}");
            assert!(
                report.contains(", 0 foundation(s) not derived yet"),
                "{report}"
            );
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
