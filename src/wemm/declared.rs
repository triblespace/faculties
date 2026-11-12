//! `faculties maintain --declared`: maintain every WeMM index the pile's
//! records name, each with the model its own descriptor names.
//!
//! A pass asks the store which WeMM indexes exist
//! ([`declared_indexes`]: the collections some record names, matched by
//! their descriptors) and how far behind each one is, both from one
//! snapshot. Indexes are grouped by the model their descriptors name (the
//! model collection, the root and the kernel profile). A group with nothing
//! to derive binds nothing. A group with work binds exactly its model, once,
//! and every index of the group is maintained with it; the runtime derives
//! only indexes of its own identity, so no index is ever computed by another
//! model. The list is never kept: the next pass asks again, so an index
//! first named by a record after start (a leaf replicated from another host,
//! or the first leaf the single-model mode derived) is maintained from then
//! on.
//!
//! A group whose model cannot be bound (a profile this build does not run,
//! a root its collection does not state or that is not here, a copy the
//! host's memory cannot admit) is skipped with one line naming its indexes,
//! and the others are maintained. It is tried again only after
//! [`BIND_RETRY_ENV`] seconds, because a bind opens the pile and may copy
//! gigabytes before it fails; the pass names that moment
//! ([`Pass::retry_at`]) and the daemon runs a pass then even if the pile has
//! not grown, since memory can come free without any write. Nothing falls
//! back to another model.
//!
//! Descriptors are read only from what is here: discovery never fetches, so
//! a collection whose descriptor has not arrived is passed over until it
//! has. Backlogs, model weights and leaves are read as maintenance reads
//! them.
//!
//! What it does not do: a descriptor that is registered but named by no
//! record is an ordinary blob, which no store indexes by what it describes,
//! so such an index is not found until its first leaf exists.
//!
//! Which indexes a host maintains is its own choice, as its merges and
//! attachments are: only those whose descriptor names a model of
//! [`MODELS_ENV`]. A declaration can never be withdrawn from a grow-only
//! store, so every retired model stays declared; a positive list keeps a
//! retired model out without anyone having to remember it, and a new index
//! of a listed model is still picked up on the next pass.
//!
//! Each pass also maintains every attached index the records name whose
//! mapping faculties defines (the Archive BM25; [`crate::maintenance`]),
//! on the CPU and with no list: the first search that kept its images
//! declared it.

use super::{Derived, Target};
use crate::storage::{AcquiringReader, FacultyStore, Storage};
use anyhow::{Context, Result};
use ed25519_dalek::SigningKey;
use std::ffi::OsStr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use triblespace::core::blob::encodings::simplearchive::SimpleArchive;
use triblespace::core::collection::{Collection, CollectionHandle, CollectionRead};
use triblespace::core::repo::BlobStoreGet;
use triblespace::core::repo::StoreRead;
use triblespace::prelude::*;
use triblespace_search::semantic_wemm::{
    collection_hex, declared_indexes, DeclaredIndex, WemmIdentity, WemmProfile,
};

/// How many models may be bound at once (default 1). Groups are served one
/// after another; a group that needs a model when this many are bound
/// releases one first, a bound one with nothing to derive if there is one,
/// else the one served least recently.
pub const MAX_BOUND_ENV: &str = "WEMM_MAINTAIN_MAX_BOUND";
/// The models whose indexes this host maintains, comma-separated:
/// `ROOT/profile`, or `COLLECTION/ROOT/profile` to name the model collection
/// too (64 hex digits). Unset, it is the one model `WEMM_ROOT`,
/// `WEMM_PROFILE` and `TRIBLESPACE_COLLECTION_MODELS` configure; with
/// neither, the declared mode refuses to start.
pub const MODELS_ENV: &str = "WEMM_MAINTAIN_MODELS";
/// Seconds before a model whose bind failed is tried again (default 1800).
pub const BIND_RETRY_ENV: &str = "WEMM_MAINTAIN_BIND_RETRY_SECS";
pub const DEFAULT_BIND_RETRY: Duration = Duration::from_secs(1800);
/// The shortest interval between two attempts to bind one model, whatever
/// is configured: a failing bind is never tried in a loop. The daemon also
/// floors the interval at its poll interval ([`Config::floor_retry`]).
pub const MIN_BIND_RETRY: Duration = Duration::from_secs(1);

/// One model of [`MODELS_ENV`]: a root under a profile, in one model
/// collection when one is named, else in any.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Listed {
    pub collection: Option<CollectionHandle>,
    pub root: Id,
    pub profile: WemmProfile,
}

impl Listed {
    fn covers(&self, identity: &WemmIdentity) -> bool {
        self.root == identity.model_root
            && self.profile.id() == identity.kernel_profile
            && self
                .collection
                .is_none_or(|collection| collection == identity.model_collection)
    }

    fn name(&self) -> String {
        match self.collection {
            Some(collection) => format!(
                "{}/{:X}/{}",
                collection_hex(collection),
                self.root,
                self.profile.label()
            ),
            None => format!("{:X}/{}", self.root, self.profile.label()),
        }
    }
}

/// The one model the single-model mode would bind: `WEMM_ROOT` under
/// `WEMM_PROFILE` in the collection `TRIBLESPACE_COLLECTION_MODELS` names,
/// or `None` when `WEMM_ROOT` is unset.
pub fn configured_model() -> Result<Option<Listed>> {
    let Ok(root) = std::env::var("WEMM_ROOT") else {
        return Ok(None);
    };
    let root = Id::from_hex(&root).context("WEMM_ROOT is not a 32-hex-digit id")?;
    let profile = super::profile_from_env_value(std::env::var_os(super::PROFILE_ENV).as_deref())?;
    let collection = crate::model_storage::models_handle()?;
    Ok(Some(Listed {
        collection: Some(collection),
        root,
        profile,
    }))
}

/// What the declared mode is configured with, read once at start.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    pub max_bound: usize,
    pub models: Vec<Listed>,
    pub retry: Duration,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Self::from_env_values(
            std::env::var_os(MAX_BOUND_ENV).as_deref(),
            std::env::var_os(MODELS_ENV).as_deref(),
            std::env::var_os(BIND_RETRY_ENV).as_deref(),
            configured_model,
        )
    }

    /// The configuration these values give, `configured` supplying the
    /// model when [`MODELS_ENV`] is unset. Anything malformed is refused,
    /// and so is a configuration that lists no model: which indexes a host
    /// maintains is never guessed.
    pub fn from_env_values(
        max_bound: Option<&OsStr>,
        models: Option<&OsStr>,
        retry: Option<&OsStr>,
        configured: impl FnOnce() -> Result<Option<Listed>>,
    ) -> Result<Self> {
        let text = |name: &str, value: &OsStr| -> Result<String> {
            value
                .to_str()
                .map(str::to_owned)
                .with_context(|| format!("{name} is not UTF-8: {value:?}"))
        };
        let max_bound = match max_bound {
            None => 1,
            Some(value) => text(MAX_BOUND_ENV, value)?
                .trim()
                .parse::<usize>()
                .ok()
                .filter(|n| *n > 0)
                .with_context(|| {
                    format!("{MAX_BOUND_ENV} must be a positive number of models; got {value:?}")
                })?,
        };
        let mut listed = Vec::new();
        match models {
            Some(value) => {
                for item in text(MODELS_ENV, value)?.split(',') {
                    let item = item.trim();
                    if !item.is_empty() {
                        listed.push(parse_listed(item)?);
                    }
                }
                anyhow::ensure!(
                    !listed.is_empty(),
                    "{MODELS_ENV} lists no model; it names ROOT/profile or COLLECTION/ROOT/profile, comma-separated"
                );
            }
            None => listed.extend(configured()?),
        }
        anyhow::ensure!(
            !listed.is_empty(),
            "faculties maintain --declared maintains only listed models, and none is: set {MODELS_ENV} (ROOT/profile or COLLECTION/ROOT/profile, comma-separated), or WEMM_ROOT and WEMM_PROFILE with TRIBLESPACE_COLLECTION_MODELS for the one model the single-model mode binds"
        );
        let retry = match retry {
            None => DEFAULT_BIND_RETRY,
            Some(value) => Duration::from_secs(
                text(BIND_RETRY_ENV, value)?
                    .trim()
                    .parse::<u64>()
                    .with_context(|| {
                        format!("{BIND_RETRY_ENV} must be a number of seconds; got {value:?}")
                    })?,
            ),
        };
        Ok(Self {
            max_bound,
            models: listed,
            retry,
        })
    }

    /// Whether this list can run with weights `aliased` from the pile or
    /// copied to the device, and the line to log at start if so. Aliased
    /// binds keep their registrations until the process exits, so binding
    /// several models in turn would accumulate them: several models need
    /// device copies. With device copies, a released model's memory is
    /// dropped with its session, but whether the allocator has returned it
    /// before the next bind reads MemAvailable has not been verified on the
    /// GPU, and the start line says so.
    pub fn check_residence(&self, aliased: bool) -> Result<Option<String>> {
        if self.models.len() <= 1 {
            return Ok(None);
        }
        anyhow::ensure!(
            !aliased,
            "{MODELS_ENV} lists {} models, and weights aliased from the pile (WEMM_COPY_WEIGHTS unset or 0) keep every bind's registrations until the process exits, so models bound in turn would accumulate them; set WEMM_COPY_WEIGHTS=1 (device copies), the supported mode for several models, or list one model",
            self.models.len()
        );
        Ok(Some(format!(
            "maintain: {} models listed, bound in turn with device copies; that the allocator returns a released model's device memory before the next bind reads MemAvailable is NOT verified on the GPU",
            self.models.len()
        )))
    }

    /// This configuration with its retry interval at least `poll`, the
    /// daemon's look interval (one second when `poll` is zero), so a retry
    /// that is due never comes round faster than the daemon looks anyway.
    pub fn floor_retry(mut self, poll: Duration) -> Self {
        let floor = if poll.is_zero() { MIN_BIND_RETRY } else { poll };
        self.retry = self.retry.max(floor);
        self
    }

    fn lists(&self, identity: &WemmIdentity) -> bool {
        self.models.iter().any(|listed| listed.covers(identity))
    }

    /// The line the declared mode starts with.
    pub fn describe(&self) -> String {
        format!(
            "maintain: every WeMM index the pile's records name whose descriptor names one of {} ({MODELS_ENV}), each with that model; at most {} bound ({MAX_BOUND_ENV}), a failed bind retried after {} s ({BIND_RETRY_ENV})",
            self.models
                .iter()
                .map(Listed::name)
                .collect::<Vec<_>>()
                .join(","),
            self.max_bound,
            self.retry.as_secs()
        )
    }
}

fn parse_listed(item: &str) -> Result<Listed> {
    let parts: Vec<&str> = item.split('/').collect();
    let (collection, root, label) = match parts.as_slice() {
        [root, label] => (None, *root, *label),
        [collection, root, label] => (
            Some(
                mary::model_collection::parse_collection_handle(collection)
                    .with_context(|| format!("{MODELS_ENV}: in {item:?}"))?,
            ),
            *root,
            *label,
        ),
        _ => anyhow::bail!("{MODELS_ENV}: {item:?} is not ROOT/profile or COLLECTION/ROOT/profile"),
    };
    let root = Id::from_hex(root)
        .with_context(|| format!("{MODELS_ENV}: {root:?} is not a 32-hex-digit model root"))?;
    let profile = WemmProfile::parse(label).with_context(|| {
        format!("{MODELS_ENV}: {label:?} is not a profile (bf16, w4a4, w4a4-vw4a16)")
    })?;
    Ok(Listed {
        collection,
        root,
        profile,
    })
}

/// How a host binds a model and maintains an index with it: the bound
/// session on a GB10 ([`SessionBinder`]), or a stand-in in tests.
pub trait Binder {
    type Model;
    /// Bind exactly the model `identity` names, or say why not.
    fn bind(&mut self, identity: WemmIdentity) -> Result<Self::Model>;
    /// Whether whatever a bind registers stays until the process exits, so
    /// that this process must never bind a second model: true for weights
    /// aliased from the pile, whose external registrations the CUDA runtime
    /// keeps until its teardown (mary `cuda_bf16_alias`), not until the
    /// session is dropped.
    fn keeps_every_bind(&self) -> bool {
        false
    }
    /// Maintain the index `target`, derived from `source`, with `model`.
    fn maintain(
        &mut self,
        model: &Self::Model,
        store: &mut FacultyStore,
        source: Collection<SimpleArchive>,
        target: Target,
        signer: &SigningKey,
        runtime: &Arc<tokio::runtime::Runtime>,
    ) -> Result<Derived>;
}

/// The bound WeMM session, with the weight residence and copy margin read
/// once at start, and the probe started for each model it binds when
/// `WEMM_PROBE_DIR` names a directory.
pub struct SessionBinder {
    pub pile: std::path::PathBuf,
    pub residence: mary::nn::cuda_bf16_alias::WeightResidence,
    pub copy_min_free: u64,
}

impl SessionBinder {
    pub fn from_env(pile: &std::path::Path) -> Result<Self> {
        use mary::nn::cuda_bf16_alias::WeightResidence;
        let residence =
            super::residence_from_env_value(std::env::var_os(super::COPY_WEIGHTS_ENV).as_deref())?;
        let copy_min_free = match residence {
            WeightResidence::DeviceCopy => super::copy_min_free_from_env_value(
                std::env::var_os(super::COPY_MIN_FREE_ENV).as_deref(),
            )?,
            WeightResidence::PileAliased => 0,
        };
        Ok(Self {
            pile: pile.to_path_buf(),
            residence,
            copy_min_free,
        })
    }
}

impl Binder for SessionBinder {
    type Model = super::Session;

    fn bind(&mut self, identity: WemmIdentity) -> Result<super::Session> {
        let session = super::Session::bind_identity(
            &self.pile,
            identity,
            self.residence,
            self.copy_min_free,
        )?;
        session.start_probe_from_env()?;
        Ok(session)
    }

    fn keeps_every_bind(&self) -> bool {
        self.residence == mary::nn::cuda_bf16_alias::WeightResidence::PileAliased
    }

    fn maintain(
        &mut self,
        model: &super::Session,
        store: &mut FacultyStore,
        source: Collection<SimpleArchive>,
        target: Target,
        signer: &SigningKey,
        runtime: &Arc<tokio::runtime::Runtime>,
    ) -> Result<Derived> {
        model.maintain(store, source, target, signer, runtime)
    }
}

/// A model, as a log line names it.
pub fn model_name(identity: &WemmIdentity) -> String {
    let profile = WemmProfile::of(identity.kernel_profile).map_or_else(
        || format!("{:X}", identity.kernel_profile),
        |p| p.label().to_owned(),
    );
    format!(
        "{:X}/{profile} in {}",
        identity.model_root,
        &collection_hex(identity.model_collection)[..16]
    )
}

fn short(handle: CollectionHandle) -> String {
    collection_hex(handle)[..16].to_owned()
}

/// A sort key for one model: the passes group indexes by it.
fn key(identity: &WemmIdentity) -> ([u8; 32], Id, Id) {
    (
        identity.model_collection.raw,
        identity.model_root,
        identity.kernel_profile,
    )
}

struct Bound<M> {
    identity: WemmIdentity,
    model: M,
    /// The pass that last maintained with it.
    served: u64,
}

/// The process side of the declared mode: the models it holds bound, the
/// binds that failed and when, and which models it has named. A few
/// entries per model; the indexes themselves are never kept.
pub struct Maintainer<B: Binder> {
    storage: Storage,
    binder: B,
    config: Config,
    bound: Vec<Bound<B::Model>>,
    failed: Vec<(WemmIdentity, Instant)>,
    named: Vec<WemmIdentity>,
    /// The first model this process bound, for a binder that keeps every
    /// bind ([`Binder::keeps_every_bind`]).
    first_bound: Option<WemmIdentity>,
    passes: u64,
    /// The attached indexes faculties defines ([`crate::maintenance`]).
    attached: crate::maintenance::AttachedUpkeep,
}

/// What one pass did: the lines to log, and when the next pass is due even
/// if the pile does not grow (the earliest bind retry still pending).
pub struct Pass {
    pub lines: Vec<String>,
    pub retry_at: Option<Instant>,
}

impl<B: Binder> Maintainer<B> {
    pub fn new(storage: Storage, binder: B, config: Config) -> Self {
        Self {
            storage,
            binder,
            config,
            bound: Vec::new(),
            failed: Vec::new(),
            named: Vec::new(),
            first_bound: None,
            passes: 0,
            attached: crate::maintenance::AttachedUpkeep::new(),
        }
    }

    /// The interval between two attempts to bind one model, from the end of
    /// the first: the configured one, never under [`MIN_BIND_RETRY`].
    fn retry(&self) -> Duration {
        self.config.retry.max(MIN_BIND_RETRY)
    }

    /// The models bound now.
    pub fn bound(&self) -> Vec<WemmIdentity> {
        self.bound.iter().map(|bound| bound.identity).collect()
    }

    /// One pass: find the indexes, bind what has work, maintain it. Returns
    /// the lines to log: an index only when the pass derived something or
    /// left something missing, a model when it is first found (listed and
    /// maintained, or not listed) and when it is skipped, bound or released.
    /// Only a failure to read the store at all is an error; one index or one
    /// model failing is a line, and the rest go on.
    pub fn pass(&mut self) -> Result<Pass> {
        self.passes += 1;
        let mut lines = Vec::new();
        // One snapshot answers which indexes exist and how far behind each
        // listed one is; an index of a model not listed is not read further.
        let config = &self.config;
        let mut declared = self.storage.with_store(|store, _, runtime| {
            // The snapshot's own reads are resident-only; the acquiring
            // reader is what maintenance reads the backlog through.
            let snapshot = store.snapshot()?;
            let reader = AcquiringReader::new(snapshot.clone(), runtime.clone());
            discover(&snapshot, &reader, config)
        })?;
        declared.sort_by_key(|(index, _)| (key(&index.identity), index.index));
        let mut groups: Vec<(WemmIdentity, Vec<(DeclaredIndex, usize)>)> = Vec::new();
        let mut unlisted: Vec<(WemmIdentity, Vec<(DeclaredIndex, usize)>)> = Vec::new();
        for (index, backlog) in declared {
            let backlog = match backlog {
                None => {
                    match unlisted.last_mut() {
                        Some((identity, members)) if *identity == index.identity => {
                            members.push((index, 0))
                        }
                        _ => unlisted.push((index.identity, vec![(index, 0)])),
                    }
                    continue;
                }
                Some(Ok(backlog)) => backlog,
                Some(Err(error)) => {
                    lines.push(format!(
                        "maintain: index {} (model {}) not read: {error:#}",
                        short(index.index),
                        model_name(&index.identity)
                    ));
                    continue;
                }
            };
            match groups.last_mut() {
                Some((identity, members)) if *identity == index.identity => {
                    members.push((index, backlog))
                }
                _ => groups.push((index.identity, vec![(index, backlog)])),
            }
        }
        // Each model is named once, when it is first found: at the first
        // pass every model then declared, later only new ones.
        for (identity, members) in &unlisted {
            if !self.named.contains(identity) {
                self.named.push(*identity);
                lines.push(format!(
                    "maintain: model {} is not listed ({MODELS_ENV}); its index(es) {} are not maintained here",
                    model_name(identity),
                    names(members)
                ));
            }
        }
        let mut work = Vec::new();
        for (identity, members) in groups {
            if !self.named.contains(&identity) {
                self.named.push(identity);
                lines.push(format!(
                    "maintain: model {} is listed; maintaining its index(es) {}",
                    model_name(&identity),
                    names(&members)
                ));
            }
            if members.iter().any(|(_, backlog)| *backlog > 0) {
                work.push((identity, members));
            }
        }
        // Models already bound first, so a bound model is not released for
        // another while it still has work of its own this pass.
        work.sort_by_key(|(identity, _)| !self.bound.iter().any(|b| b.identity == *identity));
        let wanted: Vec<WemmIdentity> = work.iter().map(|(identity, _)| *identity).collect();
        for (identity, members) in work {
            let Some(slot) = self.ensure_bound(identity, &members, &wanted, &mut lines) else {
                continue;
            };
            self.bound[slot].served = self.passes;
            for (index, _) in members {
                let model = &self.bound[slot].model;
                let binder = &mut self.binder;
                let result = self.storage.with_store(|store, signer, runtime| {
                    let snapshot = store.snapshot()?;
                    let source = Collection::<SimpleArchive>::open(&snapshot, index.source)
                        .map_err(|e| anyhow::anyhow!("open the index's source: {e}"))?;
                    let target = Target::open(&snapshot, index.index)
                        .map_err(|e| anyhow::anyhow!("open the index: {e}"))?;
                    drop(snapshot);
                    binder.maintain(model, store, source, target, signer, runtime)
                });
                match result {
                    Ok(derived) if derived.derived > 0 || derived.missing > 0 => lines.push(
                        format!("maintain: {derived} (model {})", model_name(&identity)),
                    ),
                    Ok(_) => {}
                    Err(error) => lines.push(format!(
                        "maintain: index {} (model {}) failed: {error:#}",
                        short(index.index),
                        model_name(&identity)
                    )),
                }
            }
        }
        // The attached indexes, after the models: one failing is a line.
        match self.attached.pass(&self.storage) {
            Ok(attached) => lines.extend(attached),
            Err(error) => lines.push(format!(
                "maintain: attached indexes not maintained this pass: {error:#}"
            )),
        }
        // The next attempt of every failed model that still has work, also
        // one already due (it came while later models were maintained):
        // the daemon then passes again at once, and that pass makes the
        // attempt and pushes its next one forward by the interval.
        let retry = self.retry();
        let retry_at = self
            .failed
            .iter()
            .filter(|(identity, _)| wanted.contains(identity))
            .map(|(_, ended)| *ended + retry)
            .min();
        Ok(Pass { lines, retry_at })
    }

    /// The slot of a bound model `identity`, binding it if need be; `None`
    /// when it cannot be bound now, with a line saying why.
    fn ensure_bound(
        &mut self,
        identity: WemmIdentity,
        members: &[(DeclaredIndex, usize)],
        wanted: &[WemmIdentity],
        lines: &mut Vec<String>,
    ) -> Option<usize> {
        if let Some(slot) = self.bound.iter().position(|b| b.identity == identity) {
            return Some(slot);
        }
        let now = Instant::now();
        if let Some(position) = self
            .failed
            .iter()
            .position(|(failed, _)| *failed == identity)
        {
            if now.duration_since(self.failed[position].1) < self.retry() {
                return None;
            }
            self.failed.remove(position);
        }
        if let Some(first) = self.first_bound {
            if first != identity && self.binder.keeps_every_bind() {
                // Once per retry interval, like a failed bind.
                lines.push(format!(
                    "maintain: not binding model {}: this process bound {} with weights aliased from the pile, whose registrations stay until it exits, so it binds no other model; set WEMM_COPY_WEIGHTS=1 for several models",
                    model_name(&identity),
                    model_name(&first)
                ));
                self.failed.push((identity, now));
                return None;
            }
        }
        if self.bound.len() >= self.config.max_bound {
            // A bound model with nothing to derive this pass goes first, else
            // the one served least recently.
            let release = self
                .bound
                .iter()
                .position(|b| !wanted.contains(&b.identity))
                .or_else(|| {
                    self.bound
                        .iter()
                        .enumerate()
                        .min_by_key(|(_, b)| b.served)
                        .map(|(slot, _)| slot)
                })
                .expect("a full set of bound models is not empty");
            let released = self.bound.remove(release);
            lines.push(format!(
                "maintain: releasing model {} to bind {} ({} model(s) have work, {MAX_BOUND_ENV}={})",
                model_name(&released.identity),
                model_name(&identity),
                wanted.len(),
                self.config.max_bound
            ));
            // Dropping the session drops what it owns: the runtime, its
            // tensors and the pile snapshot. That this returns device memory
            // to the host before the next bind reads MemAvailable is not
            // established: CubeCL may keep freed pool pages for reuse, and
            // nothing here asks the allocator to give them back. Aliased
            // weights' registrations stay until the process exits
            // (`Binder::keeps_every_bind`), which is why aliased binds never
            // reach this release for another model.
            drop(released);
        }
        match self.binder.bind(identity) {
            Ok(model) => {
                self.first_bound.get_or_insert(identity);
                lines.push(format!(
                    "maintain: bound model {} for index(es) {}",
                    model_name(&identity),
                    names(members)
                ));
                self.bound.push(Bound {
                    identity,
                    model,
                    served: self.passes,
                });
                Some(self.bound.len() - 1)
            }
            Err(error) => {
                // From when the attempt ended: a bind slower than the
                // interval is still due again an interval later.
                self.failed.push((identity, Instant::now()));
                lines.push(format!(
                    "maintain: skip model {}: {error:#}; index(es) {} not maintained, bind retried after {} s",
                    model_name(&identity),
                    names(members),
                    self.retry().as_secs()
                ));
                None
            }
        }
    }
}

fn names(members: &[(DeclaredIndex, usize)]) -> String {
    members
        .iter()
        .map(|(index, _)| short(index.index))
        .collect::<Vec<_>>()
        .join(",")
}

/// The WeMM indexes `resident`'s records name, from descriptors that are
/// here: descriptors are read only from `resident`, which never fetches, so
/// a collection whose descriptor is not here is passed over and nothing is
/// requested for it. Each index of a listed model comes with its backlog,
/// read through `acquiring` as maintenance reads it; other indexes are not
/// read further.
fn discover<P, A>(
    resident: &P,
    acquiring: &A,
    config: &Config,
) -> Result<Vec<(DeclaredIndex, Option<Result<usize>>)>>
where
    P: CollectionRead + BlobStoreGet,
    A: StoreRead,
{
    let mut declared = Vec::new();
    for index in declared_indexes(resident).context("find the declared WeMM indexes")? {
        let backlog = config
            .lists(&index.identity)
            .then(|| backlog(acquiring, &index));
        declared.push((index, backlog));
    }
    Ok(declared)
}

/// How many foundations of `index`'s source have no usable leaf in it.
fn backlog<R: StoreRead>(reader: &R, index: &DeclaredIndex) -> Result<usize> {
    let source = Collection::<SimpleArchive>::open(reader, index.source)
        .map_err(|e| anyhow::anyhow!("open the index's source: {e}"))?;
    let target =
        Target::open(reader, index.index).map_err(|e| anyhow::anyhow!("open the index: {e}"))?;
    crate::storage::underived(reader, source, target)
}

#[cfg(test)]
mod tests;
