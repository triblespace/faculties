//! The one model the faculties search with: WeMM-Embedding-9B, native CUDA
//! on a GB10, which places text, images and documents in one
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
//! memory and bind time, and is off unless chosen. On a GB10 that device
//! memory is host RAM no cgroup limit sees, so the bind refuses unless host
//! `MemAvailable` covers the weights plus [`COPY_MIN_FREE_ENV`]'s margin.
//! Only the payload is counted; the margin must also cover what the copy
//! costs beyond it (CubeCL's pooling and the copy-time host staging, about
//! 4.7 GiB for WeMM-9B), so a margin of 0 can overshoot `MemAvailable`.
//! Two device-copy binds on one host take turns ([`copy_lock_path`]): each
//! reads `MemAvailable` only after the one before it has finished copying.
//! [`probe`] is an optional self-check for the maintenance daemon.
//!
//! [`PROFILE_ENV`] chooses the kernel profile, and with it which index a
//! process derives and searches: `bf16` (the default) runs every projection
//! in BF16 from a BF16 root, `w4a4` runs the decoder's 248 projections as
//! NVFP4 W4A4 from a root that stores them as NVFP4 (`models_import wemm`).
//! The root and the profile are both the index's identity, so each
//! configuration has its own indexes, and a process only ever computes and
//! scores rows of the one it actually bound: a query never reads another
//! profile's index, and an index of this profile without rows is refused,
//! not answered from another. The daemon and every query read the same two
//! variables, so they agree exactly when they are configured alike.

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
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::{Duration, Instant};
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
    with_wemm_runtime, WemmIdentity, WemmIndex, WemmProfile, WemmRuntime, DEFAULT_WINDOW_CAP,
};

type Index = WemmIndex<Embedding>;
type Target = Collection<NvFp4CosineSet<Embedding>>;

/// WeMM-Embedding-9B's BF16 tensors, one alias registration each.
const ROLES: usize = 759;

/// The kernel profile: `bf16` or unset for every projection in BF16 (the
/// default), `w4a4` for the decoder's projections as NVFP4 W4A4 from a root
/// that stores them as NVFP4. Read once, when a [`Session`] binds; `WEMM_ROOT`
/// must name a root of the matching kind, or the bind is refused.
pub const PROFILE_ENV: &str = "WEMM_PROFILE";

/// The kernel profile [`PROFILE_ENV`]'s value chooses. Any value other than
/// unset, `bf16` or `w4a4` is refused, so a typo never silently selects the
/// default (and with it another index).
pub fn profile_from_env_value(value: Option<&std::ffi::OsStr>) -> Result<WemmProfile> {
    let Some(value) = value else {
        return Ok(WemmProfile::Bf16);
    };
    value.to_str().and_then(WemmProfile::parse).with_context(|| {
        format!(
            "{PROFILE_ENV} must be bf16 (every projection in BF16, the default), w4a4 (the decoder's projections as NVFP4 W4A4, from a root that stores them so) or unset; got {value:?}"
        )
    })
}

/// Whether `root` states decoder weights as stored NVFP4 leaves: what
/// decides the one profile it can be bound with.
fn stores_nvfp4_projections(facts: &TribleSet, root: Id) -> bool {
    use mary::format::attrs;
    use mary::leaf::leaf;
    use triblespace::core::blob::encodings::tensor::elements::NVFP4;
    exists!(pattern!(facts, [
        { root @ attrs::member: _?member },
        { _?member @ attrs::weight: _?weight },
        { _?weight @ leaf::<NVFP4, 2>(): _?leaf }
    ]))
}

/// Refuse a root `profile` cannot run, before any device work: a BF16 root
/// under `w4a4` (the W4A4 profile never quantizes at load, so the root names
/// the exact weights its kernels read) and an NVFP4 root under `bf16`.
fn require_root_fits(profile: WemmProfile, root: Id, stores_nvfp4: bool) -> Result<()> {
    match (profile, stores_nvfp4) {
        (WemmProfile::Bf16, false) | (WemmProfile::W4a4, true) => Ok(()),
        (WemmProfile::W4a4, false) => anyhow::bail!(
            "WEMM_ROOT {root:X} stores its decoder projections in BF16, and {PROFILE_ENV}=w4a4 binds only a root that stores them as NVFP4 (made by `models_import wemm`); name that root, or unset {PROFILE_ENV} for this one"
        ),
        (WemmProfile::Bf16, true) => anyhow::bail!(
            "WEMM_ROOT {root:X} stores its decoder projections as NVFP4, which the BF16 profile cannot run; set {PROFILE_ENV}=w4a4, or name the BF16 root"
        ),
    }
}

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

/// The bytes host `MemAvailable` must exceed the weights by before
/// [`COPY_WEIGHTS_ENV`]` = 1` may copy them; unset means
/// [`DEFAULT_COPY_MIN_FREE_BYTES`]. A decimal byte count; anything else is
/// refused rather than read as the default.
///
/// It is not only headroom. The admission counts the weights' payload bytes
/// and nothing else, while the copy costs more than that at its peak:
/// CubeCL's pool reserves device memory beyond the payload (0.91 GiB for
/// WeMM-9B), and the copy stages transient host buffers (up to 3.79 GiB
/// measured for WeMM-9B on sky, 2026-11-10). This margin must cover both, so
/// a margin below about 4.7 GiB, 0 among them, can admit a bind whose peak
/// overshoots `MemAvailable`.
pub const COPY_MIN_FREE_ENV: &str = "WEMM_COPY_MIN_FREE_BYTES";

/// 16 GiB. On a GB10 a device copy is host RAM that neither `MemoryMax` nor
/// RSS nor earlyoom sees, so a bind with copying on is admitted only while
/// `MemAvailable` covers the weights' payload bytes plus this margin. Measured
/// on sky 2026-11-10 under the daemon's CubeCL configuration (default
/// pooling) for the 16,783,407,584 payload bytes of WeMM-Embedding-9B BF16:
/// CubeCL reserved 17,758,986,752 bytes of device memory (0.91 GiB over the
/// payload), and the copy phase held two further host buffers of about
/// 2.03 GB each for about a second (3.79 GiB). That is 4.70 GiB over the
/// payload at the bind's peak. The rest, 11.3 GiB, is what the box keeps at
/// that peak: sky's earlyoom sends SIGTERM at 5% of its 121.6 GiB (6.08 GiB),
/// so the peak stays about 5 GiB above it, and 15.1 GiB stays free once the
/// transient has gone. With these weights a copy needs about 31.6 GiB.
pub const DEFAULT_COPY_MIN_FREE_BYTES: u64 = 16 << 30;

/// The margin [`COPY_MIN_FREE_ENV`]'s value chooses.
pub fn copy_min_free_from_env_value(value: Option<&std::ffi::OsStr>) -> Result<u64> {
    match value.map(|v| v.to_str()) {
        None => Ok(DEFAULT_COPY_MIN_FREE_BYTES),
        Some(Some(text)) if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) => text
            .parse()
            .with_context(|| format!("{COPY_MIN_FREE_ENV} {text:?} does not fit in 64 bits")),
        Some(other) => anyhow::bail!(
            "{COPY_MIN_FREE_ENV} must be a decimal byte count, or unset for {DEFAULT_COPY_MIN_FREE_BYTES}; got {other:?}"
        ),
    }
}

/// `MemAvailable` in bytes from the text of `/proc/meminfo`, or `None` when
/// the line is missing or unreadable.
pub fn mem_available_bytes(meminfo: &str) -> Option<u64> {
    let line = meminfo
        .lines()
        .find_map(|line| line.strip_prefix("MemAvailable:"))?;
    let kib = line.trim().strip_suffix("kB")?.trim().parse::<u64>().ok()?;
    kib.checked_mul(1024)
}

/// The admission decision for copying weights to device memory: refused
/// unless `mem_available` (bytes; `None` when it could not be read) is at
/// least `copy_bytes + min_free`. Refusal is the only alternative; nothing
/// falls back to reading the weights zero-copy.
pub fn admit_copy(
    mem_available: Option<u64>,
    copy_bytes: u64,
    min_free: u64,
) -> std::result::Result<(), String> {
    let gib = |bytes: u64| bytes as f64 / (1u64 << 30) as f64;
    let Some(available) = mem_available else {
        return Err(format!(
            "{COPY_WEIGHTS_ENV}=1 refused: host MemAvailable could not be read from /proc/meminfo, so a device copy of {copy_bytes} bytes cannot be admitted; unset {COPY_WEIGHTS_ENV} to read the weights zero-copy from the pile"
        ));
    };
    let needed = copy_bytes.checked_add(min_free);
    if needed.is_some_and(|needed| available >= needed) {
        return Ok(());
    }
    if copy_bytes == 0 {
        // The check made before any tensor is selected: only the margin.
        return Err(format!(
            "{COPY_WEIGHTS_ENV}=1 refused before copying: {COPY_MIN_FREE_ENV} alone, {min_free} bytes ({:.2} GiB) to keep free beyond the weights, exceeds host MemAvailable {available} bytes ({:.2} GiB), before any weight is counted; unset {COPY_WEIGHTS_ENV} to read the weights zero-copy from the pile",
            gib(min_free),
            gib(available)
        ));
    }
    let needed = needed.map_or_else(|| "more than 2^64".to_owned(), |n| n.to_string());
    Err(format!(
        "{COPY_WEIGHTS_ENV}=1 refused before copying: the weights need {copy_bytes} bytes ({:.2} GiB) of device memory, which on this box is host RAM, and {COPY_MIN_FREE_ENV} keeps {min_free} bytes ({:.2} GiB) free beyond them, {needed} bytes in all, but host MemAvailable is {available} bytes ({:.2} GiB); unset {COPY_WEIGHTS_ENV} to read the weights zero-copy from the pile",
        gib(copy_bytes),
        gib(min_free),
        gib(available)
    ))
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
    profile: WemmProfile,
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

/// What a bind is asked for: the model root, the kernel profile, where its
/// weights go, and for a device copy the margin [`COPY_MIN_FREE_ENV`] chose.
struct BindRequest {
    root: Id,
    profile: WemmProfile,
    residence: WeightResidence,
    copy_min_free: u64,
}

impl BindRequest {
    fn from_env() -> Result<Self> {
        let root = std::env::var("WEMM_ROOT")
            .context("WEMM_ROOT is not set; it names the WeMM model root")?;
        let root = Id::from_hex(&root).context("WEMM_ROOT is not a 32-hex-digit id")?;
        let profile = profile_from_env_value(std::env::var_os(PROFILE_ENV).as_deref())?;
        let residence = residence_from_env_value(std::env::var_os(COPY_WEIGHTS_ENV).as_deref())?;
        // Read only when copying, so a malformed margin never stops a
        // zero-copy bind.
        let copy_min_free = match residence {
            WeightResidence::DeviceCopy => {
                copy_min_free_from_env_value(std::env::var_os(COPY_MIN_FREE_ENV).as_deref())?
            }
            WeightResidence::PileAliased => 0,
        };
        Ok(Self {
            root,
            profile,
            residence,
            copy_min_free,
        })
    }
}

/// How long a device-copy bind waits for another one on this host to finish
/// copying before it is refused. A bind takes 30 to 110 s, so this is a
/// few binds; the wait never hangs on a holder that is stuck.
pub const COPY_LOCK_WAIT: Duration = Duration::from_secs(300);

/// The file every device-copy bind of this user locks:
/// `$HOME/.cache/wemm-device-copy.lock`, or `None` without `HOME`.
///
/// `MemAvailable` is read once per bind, so two binds that read it at the
/// same moment (a `faculties maintain` restart overlapping a Memory, Wiki or
/// Files command) would each be admitted against memory only one of them
/// can have, and together take twice the copy. So a bind takes an exclusive
/// `flock` on this file before it reads `MemAvailable` and keeps it until
/// its copy and the flushes after each tensor have completed, when the copy
/// is already in device memory and so in the next reader's `MemAvailable`.
/// A second bind waits up to [`COPY_LOCK_WAIT`], then is refused. A process
/// that dies releases its lock with its file descriptors. The file is never
/// removed: removing a held lock file would let the next bind lock a new
/// file beside it. One fixed rule with no fallback, so every process finds
/// the same file; it covers the processes of one user, which is every WeMM
/// process on sky. Zero-copy binds take no lock. A `HOME` that is empty or
/// relative names no fixed file (each working directory would get its own),
/// so it counts as unset.
pub fn copy_lock_path() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
        .map(|home| home.join(".cache/wemm-device-copy.lock"))
}

/// An exclusive advisory lock (`flock`) on a file that is never removed,
/// released when it is dropped.
struct CopyLock {
    _file: std::fs::File,
}

impl CopyLock {
    /// Lock `path`, creating it and its directory if need be, waiting at
    /// most `wait` while another holder has it.
    fn acquire(path: &Path, wait: Duration) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .with_context(|| format!("open the WeMM device-copy lock {}", path.display()))?;
        let started = Instant::now();
        let mut waiting = false;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Self { _file: file }),
                Err(std::fs::TryLockError::WouldBlock) => {}
                Err(std::fs::TryLockError::Error(error)) => {
                    return Err(error).with_context(|| format!("lock {}", path.display()))
                }
            }
            let waited = started.elapsed();
            if waited >= wait {
                anyhow::bail!(
                    "{COPY_WEIGHTS_ENV}=1 refused: another device copy on this host still holds {} after {:.1} s; each copy waits for the one before it, so that it reads MemAvailable after that copy is in device memory, and gives up after {} s; retry once that one has bound, or unset {COPY_WEIGHTS_ENV} to read the weights zero-copy from the pile",
                    path.display(),
                    waited.as_secs_f64(),
                    wait.as_secs_f64()
                );
            }
            if !waiting {
                eprintln!(
                    "wemm: waiting for another device copy on this host to finish ({} is held)",
                    path.display()
                );
                waiting = true;
            }
            std::thread::sleep((wait - waited).min(Duration::from_millis(100)));
        }
    }
}

/// Where a device-copy bind reads the host's memory and whom it takes turns
/// with: `/proc/meminfo` and [`copy_lock_path`], or a test's stand-ins.
struct CopyHost {
    meminfo: PathBuf,
    lock: Option<PathBuf>,
    wait: Duration,
}

impl CopyHost {
    fn this_host() -> Self {
        Self {
            meminfo: PathBuf::from("/proc/meminfo"),
            lock: copy_lock_path(),
            wait: COPY_LOCK_WAIT,
        }
    }
}

/// A device copy admitted against one `MemAvailable` reading: the margin
/// alone when it begins, then the weights as each tensor is selected. It
/// holds the host's copy lock from before the reading until it is dropped.
struct CopyAdmission {
    available: u64,
    min_free: u64,
    selected: u64,
    _lock: CopyLock,
}

impl CopyAdmission {
    /// Take the host's copy lock, then read `MemAvailable` and admit the
    /// margin alone; an unreadable `MemAvailable` is refused.
    fn begin(host: &CopyHost, min_free: u64) -> Result<Self> {
        let path = host.lock.as_deref().with_context(|| {
            format!("{COPY_WEIGHTS_ENV}=1 refused: HOME is not set, so the device-copy lock $HOME/.cache/wemm-device-copy.lock cannot be found")
        })?;
        let lock = CopyLock::acquire(path, host.wait)?;
        let available = std::fs::read_to_string(&host.meminfo)
            .ok()
            .and_then(|text| mem_available_bytes(&text));
        admit_copy(available, 0, min_free).map_err(anyhow::Error::msg)?;
        Ok(Self {
            // admit_copy has refused an unreadable reading; 0 would refuse
            // every selection anyway.
            available: available.unwrap_or(0),
            min_free,
            selected: 0,
            _lock: lock,
        })
    }

    /// Count one more selected tensor's payload bytes, refused once the
    /// weights selected so far no longer fit the reading.
    fn select(&mut self, bytes: u64) -> std::result::Result<(), String> {
        self.selected = self.selected.saturating_add(bytes);
        admit_copy(Some(self.available), self.selected, self.min_free)
    }
}

impl Session {
    /// Bind the model `WEMM_ROOT` names from the `models` collection of the
    /// pile at `pile`, the pile its indexes are written to as well.
    pub fn from_env(pile: &Path) -> Result<Self> {
        Self::bind(pile, BindRequest::from_env()?, &CopyHost::this_host())
    }

    fn bind(pile: &Path, request: BindRequest, host: &CopyHost) -> Result<Self> {
        let BindRequest {
            root,
            profile,
            residence,
            copy_min_free,
        } = request;
        // A device copy is admitted against MemAvailable read once, here,
        // before anything is bound. The margin alone is checked now (and an
        // unreadable MemAvailable refused); the weights' bytes are added as
        // each tensor is selected below. Every selection precedes the first
        // copy, and in any order a copy follows its own selection, so the
        // bytes copied never exceed what this reading admitted.
        let mut admission = match residence {
            WeightResidence::DeviceCopy => Some(CopyAdmission::begin(host, copy_min_free)?),
            WeightResidence::PileAliased => None,
        };
        let model = pile.to_path_buf();
        let backing = BackingIdentity::of(&model)?;
        let Resolved {
            snapshot,
            root,
            assets,
            ..
        } = resolve(&model, crate::model_storage::models_handle()?, root)?;
        require_root_fits(
            profile,
            root,
            stores_nvfp4_projections(snapshot.facts(), root),
        )?;
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
            NativeWemm::from_frozen_with(
                snapshot.facts(),
                snapshot.store(),
                root,
                assets,
                &mut aliases,
                |_, _, _, payload| {
                    roles += 1;
                    // A stored NVFP4 leaf's payload is its packed planes,
                    // which is what goes to device memory.
                    if let Some(admission) = admission.as_mut() {
                        admission.select(payload.len() as u64).map_err(|e| {
                            format!("after selecting {roles} of {ROLES} tensors: {e}")
                        })?;
                    }
                    Ok(())
                },
                profile.precision(),
            )
        }?;
        if let Some(admission) = &admission {
            // Every copy follows its own selection, so nothing copied went
            // uncounted; a bind that copied more than it admitted says so.
            let copied = aliases.stats().copied_bytes;
            anyhow::ensure!(
                copied <= admission.selected,
                "{COPY_WEIGHTS_ENV}=1 copied {copied} bytes to device memory but admitted only {} selected bytes",
                admission.selected
            );
            eprintln!(
                "wemm: copy admitted: {} bytes of weights + {} ({COPY_MIN_FREE_ENV}) <= MemAvailable {} bytes at bind start",
                admission.selected, admission.min_free, admission.available
            );
        }
        backing.require_still(&model)?;
        // The identity is read from what was bound, and must be what was
        // asked for: never a label put on a model that runs otherwise.
        let runtime = WemmRuntime::from_native(native, codec)?;
        let identity = runtime.identity();
        anyhow::ensure!(
            identity.kernel_profile == profile.id() && identity.model_root == root,
            "WEMM_ROOT {root:X} under {PROFILE_ENV}={} bound as root {:X}, profile {:X}",
            profile.label(),
            identity.model_root,
            identity.kernel_profile
        );
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
        eprintln!(
            "wemm: kernel profile {} ({:X}), model root {:X}",
            profile.label(),
            identity.kernel_profile,
            identity.model_root
        );
        // The copy, and the two flushes after each tensor that free its host
        // staging, completed inside from_frozen_with, so the next bind's
        // MemAvailable reading already sees them: only now may it read.
        drop(admission);
        Ok(Self {
            runtime: Rc::new(RefCell::new(runtime)),
            identity,
            profile,
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

    /// The model root and kernel profile this session actually bound: the
    /// identity of every index it derives or searches.
    pub fn identity(&self) -> WemmIdentity {
        self.identity
    }

    /// The kernel profile this session bound.
    pub fn profile(&self) -> WemmProfile {
        self.profile
    }

    /// Start the [`probe`] self-check if [`probe::PROBE_DIR_ENV`] names a
    /// directory: probe once now, then after every
    /// [`probe::PROBE_EVERY_FOUNDATIONS`] foundations this thread derives.
    /// Returns whether it is on. Only the setup can fail; a later probe that
    /// cannot run is logged and skipped, never an error of the derive.
    ///
    /// The latest call decides this thread's probe: whatever probe ran before
    /// is stopped first, also when this call leaves the probe off. The probe
    /// never keeps the model bound; it ends with its [`Session`].
    pub fn start_probe_from_env(&self) -> Result<bool> {
        PROBE.with(|slot| *slot.borrow_mut() = None);
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
                "kernel_profile",
                format!(
                    "{} {:X}",
                    self.profile.label(),
                    self.identity.kernel_profile
                ),
            )
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
        PROBE.with(|slot| install_probe(slot, checker, &self.runtime));
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
                profile: self.profile,
            });
        }
        let cosines = with_wemm_runtime(Rc::clone(&self.runtime), || match query {
            Query::Text(text) => mapping.reconstructed_cosines(&index, text),
            Query::Content(bytes) => mapping.reconstructed_cosines_content(&index, bytes),
        })??;
        Ok(Scores {
            cosines,
            missing,
            profile: self.profile,
        })
    }
}

impl Drop for Session {
    /// Stop this session's probe, so nothing of the session outlives it on
    /// this thread. During thread teardown the slot may already be gone.
    fn drop(&mut self) {
        let _ = PROBE.try_with(|slot| release_probe(slot, &self.runtime));
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
    /// The profile of the index these rows are from, the session's own.
    pub profile: WemmProfile,
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

    /// The refusal for an index with no rows at all. It names the profile:
    /// an index of another profile is never searched in its place.
    pub fn require_rows(&self, faculty: &str) -> Result<()> {
        anyhow::ensure!(
            !self.cosines.is_empty(),
            "the {faculty} WeMM index of the {} profile ({PROFILE_ENV}) has no rows yet ({} commit(s) to derive); `faculties maintain` configured alike derives them on a GB10",
            self.profile.label(),
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

/// The running self-check. It holds its session's runtime weakly: the model
/// stays bound exactly as long as the [`Session`] that started the probe.
struct Probe<R = WemmRuntime> {
    checker: probe::Checker,
    runtime: Weak<RefCell<R>>,
}

/// Put a probe on `runtime` in `slot`, replacing (and dropping) any probe
/// that was there.
fn install_probe<R>(
    slot: &RefCell<Option<Probe<R>>>,
    checker: probe::Checker,
    runtime: &Rc<RefCell<R>>,
) {
    *slot.borrow_mut() = Some(Probe {
        checker,
        runtime: Rc::downgrade(runtime),
    });
}

/// Clear `slot` if its probe is on `runtime`. A probe of another session is
/// left alone. A slot that is in use is left too: the weak reference already
/// keeps it from holding the model.
fn release_probe<R>(slot: &RefCell<Option<Probe<R>>>, runtime: &Rc<RefCell<R>>) {
    let Ok(mut slot) = slot.try_borrow_mut() else {
        return;
    };
    if slot
        .as_ref()
        .is_some_and(|probe| Weak::ptr_eq(&probe.runtime, &Rc::downgrade(runtime)))
    {
        *slot = None;
    }
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
        let Some(runtime) = probe.runtime.upgrade() else {
            eprintln!("wemm probe: off, its session has ended");
            *slot = None;
            return;
        };
        let output = match runtime.try_borrow_mut() {
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

    /// A stand-in for the bound runtime that records when it is dropped.
    struct Model(Rc<Cell<bool>>);

    impl Drop for Model {
        fn drop(&mut self) {
            self.0.set(true);
        }
    }

    fn model() -> (Rc<RefCell<Model>>, Rc<Cell<bool>>) {
        let dropped = Rc::new(Cell::new(false));
        (Rc::new(RefCell::new(Model(dropped.clone()))), dropped)
    }

    fn checker(dir: &Path) -> probe::Checker {
        let identity = probe::Provenance::default()
            .with("model_root", "85B6ACD39E6CB93EE8BA31FDEF6C4F4F")
            .with("probe_ids_sha256", "0123456789abcdef0123");
        probe::Checker::new(dir, identity, 4).unwrap()
    }

    #[test]
    fn a_probe_never_keeps_its_sessions_model_alive() {
        let dir = tempfile::tempdir().unwrap();
        let slot = RefCell::new(None::<Probe<Model>>);
        let (first, first_dropped) = model();
        install_probe(&slot, checker(dir.path()), &first);
        // The session ends while its probe is still installed.
        drop(first);
        assert!(first_dropped.get(), "the probe kept the model alive");

        // A later session's probe replaces it and is just as weak.
        let (second, second_dropped) = model();
        install_probe(&slot, checker(dir.path()), &second);
        drop(second);
        assert!(second_dropped.get(), "the probe kept the model alive");
    }

    #[test]
    fn a_session_releases_only_its_own_probe() {
        let dir = tempfile::tempdir().unwrap();
        let slot = RefCell::new(None::<Probe<Model>>);
        let (mine, _) = model();
        let (other, _) = model();
        install_probe(&slot, checker(dir.path()), &mine);
        release_probe(&slot, &other);
        assert!(
            slot.borrow().is_some(),
            "another session cleared this probe"
        );
        release_probe(&slot, &mine);
        assert!(slot.borrow().is_none(), "the session's own probe stayed");
    }

    #[test]
    fn the_profile_is_bf16_unless_w4a4_is_chosen_exactly() {
        use std::ffi::OsStr;
        use triblespace_search::semantic_wemm::{
            GB10_NATIVE_BF16_PROFILE, GB10_NATIVE_W4A4_PROFILE,
        };
        // Unset is the BF16 profile, whose id every live index was derived
        // under: an unconfigured process finds exactly the indexes it found.
        let unset = profile_from_env_value(None).unwrap();
        assert_eq!(unset, WemmProfile::Bf16);
        assert_eq!(
            unset.id(),
            Id::from_hex("56AA6AA44ECDFF68B91CA45E39BC9EB6").unwrap()
        );
        assert_eq!(
            profile_from_env_value(Some(OsStr::new("bf16"))).unwrap(),
            WemmProfile::Bf16
        );
        let w4a4 = profile_from_env_value(Some(OsStr::new("w4a4"))).unwrap();
        assert_eq!(w4a4, WemmProfile::W4a4);
        assert_eq!(w4a4.id(), GB10_NATIVE_W4A4_PROFILE);
        assert_ne!(GB10_NATIVE_W4A4_PROFILE, GB10_NATIVE_BF16_PROFILE);
        for typo in ["", "W4A4", "fp4", " w4a4", "w4a16", "nvfp4", "BF16", "0"] {
            let error = profile_from_env_value(Some(OsStr::new(typo))).unwrap_err();
            assert!(error.to_string().contains(PROFILE_ENV), "{error}");
        }
    }

    /// The root decides the profile it can be bound with: a BF16 root only
    /// under bf16, a root storing NVFP4 projections only under w4a4.
    #[test]
    fn a_root_binds_only_under_the_profile_of_what_it_stores() {
        use mary::format::attrs;
        use mary::leaf::leaf;
        use triblespace::core::blob::encodings::tensor::elements::{BF16, NVFP4};
        use triblespace::core::inline::encodings::hash::Handle;
        use triblespace::core::trible::Trible;
        let root = Id::new([1; 16]).unwrap();
        let (member, weight) = (Id::new([2; 16]).unwrap(), Id::new([3; 16]).unwrap());
        let gen = |id: Id| -> Inline<triblespace::core::inline::encodings::genid::GenId> {
            id.to_inline()
        };
        let mut bf16 = TribleSet::new();
        bf16.insert(&Trible::force(&root, &attrs::member.id(), &gen(member)));
        bf16.insert(&Trible::force(&member, &attrs::weight.id(), &gen(weight)));
        let mut nvfp4 = bf16.clone();
        bf16.insert(&Trible::force(
            &weight,
            &leaf::<BF16, 2>().id(),
            &Inline::<Handle<triblespace::core::blob::encodings::tensor::Tensor<BF16, 2>>>::new(
                [7; 32],
            ),
        ));
        nvfp4.insert(&Trible::force(
            &weight,
            &leaf::<NVFP4, 2>().id(),
            &Inline::<Handle<triblespace::core::blob::encodings::tensor::Tensor<NVFP4, 2>>>::new(
                [8; 32],
            ),
        ));
        assert!(!stores_nvfp4_projections(&bf16, root));
        assert!(stores_nvfp4_projections(&nvfp4, root));
        // Another root's NVFP4 leaves say nothing about this one.
        assert!(!stores_nvfp4_projections(&nvfp4, Id::new([9; 16]).unwrap()));

        assert!(require_root_fits(WemmProfile::Bf16, root, false).is_ok());
        assert!(require_root_fits(WemmProfile::W4a4, root, true).is_ok());
        let error = require_root_fits(WemmProfile::W4a4, root, false).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("stores its decoder projections in BF16"),
            "{error}"
        );
        let error = require_root_fits(WemmProfile::Bf16, root, true).unwrap_err();
        assert!(error.to_string().contains("as NVFP4"), "{error}");
        assert!(error.to_string().contains(PROFILE_ENV), "{error}");
    }

    #[test]
    fn an_empty_index_is_refused_naming_its_profile() {
        let scores = Scores {
            cosines: ReconstructedCosines::default(),
            missing: 3,
            profile: WemmProfile::W4a4,
        };
        let error = scores.require_rows("Memory").unwrap_err().to_string();
        assert!(error.contains("w4a4 profile"), "{error}");
        assert!(error.contains(PROFILE_ENV), "{error}");
    }

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

    /// WeMM-Embedding-9B BF16's payload bytes (byte check, 2026-11-10).
    const WEMM_PAYLOAD: u64 = 16_783_407_584;

    #[test]
    fn a_copy_is_refused_when_memavailable_is_short_of_the_weights_plus_the_margin() {
        let margin = DEFAULT_COPY_MIN_FREE_BYTES;
        let error = admit_copy(Some(WEMM_PAYLOAD + margin - 1), WEMM_PAYLOAD, margin).unwrap_err();
        // Both numbers are named: what is needed and what is available.
        assert!(
            error.contains(&(WEMM_PAYLOAD + margin).to_string()),
            "{error}"
        );
        assert!(
            error.contains(&(WEMM_PAYLOAD + margin - 1).to_string()),
            "{error}"
        );
        assert!(error.contains(&WEMM_PAYLOAD.to_string()), "{error}");
        assert!(error.contains(COPY_MIN_FREE_ENV), "{error}");
        // A box with far less than the weights alone.
        assert!(admit_copy(Some(8 << 30), WEMM_PAYLOAD, margin).is_err());
        // The margin alone can refuse, before any tensor is selected.
        assert!(admit_copy(Some(margin - 1), 0, margin).is_err());
        // One byte over the weights is still a weights refusal.
        let error = admit_copy(Some(margin), 1, margin).unwrap_err();
        assert!(error.contains("the weights need 1 bytes"), "{error}");
        // A sum past 2^64 is refused, not wrapped into a small number.
        assert!(admit_copy(Some(u64::MAX), u64::MAX, 1).is_err());
    }

    #[test]
    fn the_margin_alone_refusal_says_what_was_compared() {
        let margin = DEFAULT_COPY_MIN_FREE_BYTES;
        let error = admit_copy(Some(margin - 1), 0, margin).unwrap_err();
        // No weights were counted yet, so none are named.
        assert!(!error.contains("need 0 bytes"), "{error}");
        assert!(
            error.contains(&format!("{COPY_MIN_FREE_ENV} alone")),
            "{error}"
        );
        assert!(error.contains(&margin.to_string()), "{error}");
        assert!(error.contains(&(margin - 1).to_string()), "{error}");
    }

    #[test]
    fn a_copy_is_admitted_with_exactly_enough_or_plenty() {
        let margin = DEFAULT_COPY_MIN_FREE_BYTES;
        assert_eq!(
            admit_copy(Some(WEMM_PAYLOAD + margin), WEMM_PAYLOAD, margin),
            Ok(())
        );
        assert_eq!(admit_copy(Some(120 << 30), WEMM_PAYLOAD, margin), Ok(()));
        assert_eq!(admit_copy(Some(margin), 0, margin), Ok(()));
    }

    #[test]
    fn an_unreadable_memavailable_refuses_the_copy() {
        let error = admit_copy(None, WEMM_PAYLOAD, 0).unwrap_err();
        assert!(error.contains("could not be read"), "{error}");
        assert!(admit_copy(None, 0, 0).is_err());
        for broken in [
            "",
            "MemTotal:       127533080 kB\nMemFree:  1 kB\n",
            "MemAvailable:   lots kB\n",
            "MemAvailable:   87084912\n",
            "MemAvailable:   87084912 MB\n",
            "MemAvailable: 18446744073709551615 kB\n",
        ] {
            assert_eq!(mem_available_bytes(broken), None, "{broken:?}");
        }
        assert_eq!(
            mem_available_bytes(
                "MemTotal:       127533080 kB\nMemFree:        50331648 kB\nMemAvailable:   87084912 kB\nBuffers: 1 kB\n"
            ),
            Some(87_084_912 * 1024)
        );
    }

    #[test]
    fn the_copy_margin_is_a_byte_count_or_its_default() {
        use std::ffi::OsStr;
        assert_eq!(copy_min_free_from_env_value(None).unwrap(), 16 << 30);
        assert_eq!(
            copy_min_free_from_env_value(Some(OsStr::new("0"))).unwrap(),
            0
        );
        assert_eq!(
            copy_min_free_from_env_value(Some(OsStr::new("34359738368"))).unwrap(),
            32 << 30
        );
        for typo in ["", "16G", "-1", " 1", "1e9", "99999999999999999999"] {
            let error = copy_min_free_from_env_value(Some(OsStr::new(typo))).unwrap_err();
            assert!(error.to_string().contains(COPY_MIN_FREE_ENV), "{error}");
        }
    }

    /// A host whose `/proc/meminfo` stand-in, `name` in `dir`, reports
    /// `available_kib` as MemAvailable.
    fn copy_host(dir: &Path, name: &str, available_kib: u64) -> CopyHost {
        let meminfo = dir.join(name);
        std::fs::write(
            &meminfo,
            format!("MemTotal:       127533080 kB\nMemAvailable:   {available_kib} kB\n"),
        )
        .unwrap();
        CopyHost {
            meminfo,
            lock: Some(dir.join("cache/wemm-device-copy.lock")),
            wait: Duration::from_millis(200),
        }
    }

    fn request(residence: WeightResidence, copy_min_free: u64) -> BindRequest {
        BindRequest {
            root: Id::from_hex("85B6ACD39E6CB93EE8BA31FDEF6C4F4F").unwrap(),
            profile: WemmProfile::Bf16,
            residence,
            copy_min_free,
        }
    }

    fn bind_error(pile: &Path, request: BindRequest, host: &CopyHost) -> String {
        match Session::bind(pile, request, host) {
            Ok(_) => panic!("bound a model from a pile that does not exist"),
            Err(error) => format!("{error:#}"),
        }
    }

    /// The admission is wired into the bind: a device copy whose margin
    /// MemAvailable cannot cover is refused before the pile is even looked
    /// at, so before any device work. The pile here does not exist, and
    /// stat-ing it is the bind's first step after the admission.
    #[test]
    fn a_device_copy_bind_refuses_before_the_pile_or_the_device() {
        let dir = tempfile::tempdir().unwrap();
        let pile = dir.path().join("absent.pile");
        let margin = DEFAULT_COPY_MIN_FREE_BYTES;
        let short = copy_host(dir.path(), "short", 8 << 20); // 8 GiB
        let error = bind_error(&pile, request(WeightResidence::DeviceCopy, margin), &short);
        assert!(error.contains("refused before copying"), "{error}");
        assert!(
            error.contains(&format!("{COPY_MIN_FREE_ENV} alone")),
            "{error}"
        );
        assert!(
            !error.contains("absent.pile"),
            "it reached the pile: {error}"
        );

        // An unreadable MemAvailable refuses the same way.
        let unreadable = CopyHost {
            meminfo: dir.path().join("no-such-meminfo"),
            ..copy_host(dir.path(), "unused", 0)
        };
        let error = bind_error(&pile, request(WeightResidence::DeviceCopy, 0), &unreadable);
        assert!(error.contains("could not be read"), "{error}");
        assert!(
            !error.contains("absent.pile"),
            "it reached the pile: {error}"
        );

        // Controls: with room the copy is admitted and the bind goes on to
        // the pile, and a zero-copy bind never asks.
        let roomy = copy_host(dir.path(), "roomy", 1 << 30); // 1 TiB
        for (residence, host) in [
            (WeightResidence::DeviceCopy, &roomy),
            (WeightResidence::PileAliased, &short),
        ] {
            let error = bind_error(&pile, request(residence, margin), host);
            assert!(
                error.contains("stat") && error.contains("absent.pile"),
                "{error}"
            );
        }

        // Every bind above released the copy lock when it returned, and a
        // copy waits for one that holds it while a zero-copy bind does not.
        let lock = roomy.lock.as_deref().unwrap();
        let other = CopyLock::acquire(lock, Duration::ZERO).unwrap();
        let error = bind_error(&pile, request(WeightResidence::DeviceCopy, margin), &roomy);
        assert!(error.contains("still holds"), "{error}");
        let error = bind_error(&pile, request(WeightResidence::PileAliased, margin), &roomy);
        assert!(error.contains("absent.pile"), "{error}");
        drop(other);
    }

    /// Two contenders open the lock file separately, so they hold separate
    /// open file descriptions: for `flock` that is the same as two processes.
    #[test]
    fn a_second_device_copy_waits_for_the_first_and_then_gives_up_clearly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache/wemm-device-copy.lock");
        let first = CopyLock::acquire(&path, Duration::ZERO).unwrap();
        let started = Instant::now();
        let Err(error) = CopyLock::acquire(&path, Duration::from_millis(300)) else {
            panic!("two device copies held the lock at once");
        };
        assert!(started.elapsed() >= Duration::from_millis(300));
        let error = format!("{error:#}");
        assert!(error.contains(COPY_WEIGHTS_ENV), "{error}");
        assert!(error.contains(&path.display().to_string()), "{error}");
        drop(first);
        // Released, never removed: the next copy locks the same file.
        assert!(path.exists());
        let _next = CopyLock::acquire(&path, Duration::ZERO).unwrap();
    }

    #[test]
    fn a_waiting_device_copy_proceeds_once_the_first_has_finished() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wemm-device-copy.lock");
        let (held, is_held) = std::sync::mpsc::channel();
        let released = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let holder = {
            let path = path.clone();
            let released = released.clone();
            std::thread::spawn(move || {
                let lock = CopyLock::acquire(&path, Duration::ZERO).unwrap();
                held.send(()).unwrap();
                std::thread::sleep(Duration::from_millis(400));
                released.store(true, std::sync::atomic::Ordering::SeqCst);
                drop(lock);
            })
        };
        is_held.recv().unwrap();
        let _lock = CopyLock::acquire(&path, Duration::from_secs(30)).unwrap();
        // Ordering, not wall time: the waiter holds the lock only after the
        // first copy has let it go.
        assert!(
            released.load(std::sync::atomic::Ordering::SeqCst),
            "it did not wait for the first copy"
        );
        holder.join().unwrap();
    }

    /// The lock spans the reading: an admission takes it before it reads
    /// MemAvailable (with the lock held elsewhere, even an unreadable
    /// meminfo is not reached) and keeps it until it is dropped.
    #[test]
    fn an_admission_holds_the_copy_lock_from_before_its_reading_until_it_ends() {
        let dir = tempfile::tempdir().unwrap();
        let host = copy_host(dir.path(), "meminfo", 1 << 30);
        let lock = host.lock.clone().unwrap();
        let unreadable = CopyHost {
            meminfo: dir.path().join("no-such-meminfo"),
            ..copy_host(dir.path(), "unused", 0)
        };
        let other = CopyLock::acquire(&lock, Duration::ZERO).unwrap();
        let Err(error) = CopyAdmission::begin(&unreadable, 0) else {
            panic!("admitted with no MemAvailable");
        };
        let error = format!("{error:#}");
        assert!(error.contains("still holds"), "{error}");
        assert!(CopyAdmission::begin(&host, 0).is_err());
        drop(other);

        let admission = CopyAdmission::begin(&host, 0).unwrap();
        assert!(
            CopyLock::acquire(&lock, Duration::ZERO).is_err(),
            "an admitted copy did not hold the lock"
        );
        drop(admission);
        CopyLock::acquire(&lock, Duration::ZERO).unwrap();
    }

    #[test]
    fn a_selection_past_the_reading_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let host = copy_host(dir.path(), "meminfo", 1 << 20); // 1 GiB
        let mut admission = CopyAdmission::begin(&host, 512 << 20).unwrap();
        assert_eq!(admission.select(256 << 20), Ok(()));
        // Exactly MemAvailable: weights plus margin.
        assert_eq!(admission.select(256 << 20), Ok(()));
        let error = admission.select(1).unwrap_err();
        assert!(error.contains(&((1u64 << 30) + 1).to_string()), "{error}");
        assert!(error.contains("the weights need"), "{error}");
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
    /// The W4A4 profile on the real stored NVFP4 root. Needs a GB10 under the
    /// gb10 lock, `WEMM_W4A4_PILE` (a WRITABLE copy of a pile whose `models`
    /// collection `WEMM_GATE_MODELS` (64 hex digits; set as
    /// `TRIBLESPACE_COLLECTION_MODELS` after the ambient routes are cleared)
    /// holds the NVFP4 root
    /// `WEMM_ROOT`, alone in its directory: the test initializes a signing
    /// key beside it and writes Memory entries and their index into it), and
    /// optionally `WEMM_GATE_PNG` (a PNG to journal as a wordless image).
    ///
    /// Checks, in order: the BF16 profile refuses the NVFP4 root before any
    /// device work; a W4A4 bind's identity is the root and the W4A4 profile;
    /// a row is the same alone and in a batch; Memory derives completely
    /// under the W4A4 descriptor, which binds back to that identity and is
    /// not the BF16 descriptor; a query finds what it should; and a mapping
    /// of the BF16 profile neither computes here nor is scored by this
    /// runtime.
    #[test]
    #[ignore = "requires a GB10, a writable pile holding the stored NVFP4 WeMM root"]
    fn a_w4a4_bind_on_the_stored_root_derives_rows_under_its_own_descriptor() {
        use crate::memory::operations::parse_time_range;
        use crate::out::{Out, Part};
        use triblespace::core::blob::encodings::rawbytes::RawBytes;
        use triblespace::core::blob::{IntoBlob, TryFromBlob};
        use triblespace::core::inline::encodings::hash::Handle;
        use triblespace::core::repo::{BlobStoreGet, SnapshotSource};
        use triblespace::core::trible::Trible;
        use triblespace_search::semantic_wemm::GB10_NATIVE_W4A4_PROFILE;

        let pile = PathBuf::from(std::env::var("WEMM_W4A4_PILE").unwrap());
        let models = std::env::var("WEMM_GATE_MODELS").unwrap();
        let root = Id::from_hex(&std::env::var("WEMM_ROOT").unwrap()).unwrap();
        let png = std::env::var_os("WEMM_GATE_PNG").map(|p| std::fs::read(p).unwrap());
        crate::test_support::clear_ambient_environment();
        std::env::set_var("TRIBLESPACE_COLLECTION_MODELS", models);
        crate::storage::initialize_signer(&pile, None).unwrap();
        let host = CopyHost::this_host();
        let bind = |profile| {
            Session::bind(
                &pile,
                BindRequest {
                    root,
                    profile,
                    residence: WeightResidence::PileAliased,
                    copy_min_free: 0,
                },
                &host,
            )
        };

        let Err(refused) = bind(WemmProfile::Bf16) else {
            panic!("the BF16 profile bound a root that stores NVFP4 projections");
        };
        let refused = format!("{refused:#}");
        eprintln!("refused bf16 on the NVFP4 root: {refused}");
        assert!(refused.contains("as NVFP4"), "{refused}");

        let started = Instant::now();
        let session = bind(WemmProfile::W4a4).unwrap();
        eprintln!("wemm w4a4 bind {:.1} s", started.elapsed().as_secs_f64());
        assert_eq!(
            session.identity(),
            WemmIdentity {
                model_root: root,
                kernel_profile: GB10_NATIVE_W4A4_PROFILE,
            }
        );

        // Every row is computed independently: a text's row alone and in a
        // batch of four are the same bytes (after the early forwards).
        {
            let mut runtime = session.runtime.borrow_mut();
            for _ in 0..probe::EARLY_FORWARDS {
                runtime.probe_texts(&probe::PROBE_TEXTS).unwrap();
            }
            let batch = runtime.probe_texts(&probe::PROBE_TEXTS).unwrap().raw;
            for (at, text) in probe::PROBE_TEXTS.iter().enumerate() {
                let alone = runtime.probe_texts(&[text]).unwrap().raw;
                assert_eq!(
                    alone,
                    batch[at * 8192..(at + 1) * 8192],
                    "text {at}: its row alone differs from its row in the batch"
                );
            }
            eprintln!(
                "identity batch-invariant rows: {} texts alone == in their batch",
                probe::PROBE_TEXTS.len()
            );
        }

        let storage = crate::storage::Storage::shared(pile.clone(), None);
        let memory = crate::memory::Memory::with_storage(storage.clone());
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
        let crab = png
            .as_ref()
            .map(|png| memory.image(png, day(5)).unwrap().id);

        let started = Instant::now();
        let derived = memory.derive_wemm(&session).unwrap();
        eprintln!(
            "derived {derived} ({:.1} s)",
            started.elapsed().as_secs_f64()
        );
        assert_eq!(derived.missing, 0, "{derived}");
        assert!(
            derived.rows >= texts.len() + usize::from(crab.is_some()),
            "{derived}"
        );

        // The index derived into is the W4A4 descriptor: it binds back to the
        // session's identity, and the BF16 descriptor of the same root and
        // content is another descriptor.
        let snapshot = mary::persist::read_model_pile_snapshot_read_only(&pile).unwrap();
        let blob: triblespace::core::blob::Blob<SimpleArchive> =
            snapshot.get(derived.index).unwrap();
        let described = Fragment::from(TribleSet::try_from_blob(blob).unwrap());
        let mapping = Index::bind(&Fragment::empty(), &described).unwrap();
        assert_eq!(mapping.identity(), session.identity());
        eprintln!(
            "index {} descriptor binds to root {:X} profile {:X}",
            hex::encode(derived.index.raw),
            mapping.identity().model_root,
            mapping.identity().kernel_profile
        );
        let bf16 = Index::new(
            mapping.content_attributes(),
            WemmIdentity {
                model_root: root,
                kernel_profile: WemmProfile::Bf16.id(),
            },
            DEFAULT_WINDOW_CAP,
        )
        .unwrap();
        assert_ne!(bf16.fragment().root(), mapping.fragment().root());

        let mut report = String::new();
        let mut emit = |part: Part| {
            if let Part::Text { text } = part {
                report.push_str(&text);
            }
            Ok(())
        };
        memory
            .similar_with(&session, "baking bread at home", &mut Out::new(&mut emit))
            .unwrap();
        eprintln!("query: {report}");
        assert!(report
            .lines()
            .next()
            .unwrap_or_default()
            .contains(&format!("{:x}", chunks[1])));

        // A mapping of another profile is neither computable here nor
        // computed: this runtime refuses it rather than embed under W4A4.
        let mut facts = TribleSet::new();
        let content = mapping.content_attributes().next().unwrap();
        facts.insert(&Trible::force(
            &Id::new([1; 16]).unwrap(),
            &content,
            &Inline::<Handle<RawBytes>>::new([5; 32]),
        ));
        let mut repo = triblespace::core::repo::memoryrepo::MemoryRepo::default();
        let reader = repo.snapshot().unwrap();
        let (computable, refusal) = with_wemm_runtime(Rc::clone(&session.runtime), || {
            (
                (bf16.computable_here(), mapping.computable_here()),
                bf16.map(&facts.to_blob(), &reader).unwrap_err().to_string(),
            )
        })
        .unwrap();
        eprintln!("refused bf16 mapping under the w4a4 runtime: {refusal}");
        assert_eq!(computable, (false, true));
        assert!(refusal.contains("does not match"), "{refusal}");
        storage.close().unwrap();
    }
}
