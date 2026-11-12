//! `faculties maintain --declared`: maintain every WeMM index the pile's
//! records name, each with the model its own descriptor names, through one
//! maintenance registration ([`Registration`]) that an application's
//! maintenance Driver can run as well (`Driver::with_registered`).
//!
//! At the start of each pass the registration asks the store which WeMM
//! indexes exist (`declared_indexes`: the collections some record names whose
//! descriptor is a complete WeMM descriptor) and how far
//! behind each one is, both from one snapshot. Indexes are grouped by the
//! model their descriptors name (the model collection, the root and the
//! kernel profile). An index with nothing to derive binds nothing. An index
//! with work binds exactly its model, once, and keeps it bound; the runtime
//! derives only indexes of its own identity, so no index is ever computed by
//! another model. The list is never kept: the next pass asks again, so an
//! index first named by a record after start (a leaf replicated from another
//! host, or the first leaf the single-model mode derived) is maintained from
//! then on.
//!
//! A model that cannot be bound (a profile this build does not run, a root
//! its collection does not state or that is not here, a copy the host's
//! memory cannot admit) is skipped with one line naming its indexes, and the
//! others are maintained. It is tried again only after [`BIND_RETRY_ENV`]
//! seconds, because a bind opens the pile and may copy gigabytes before it
//! fails; the registration names that moment (`Mappings::due`) and
//! the daemon runs a pass then even if the pile has not grown, since memory
//! can come free without any write. Nothing falls back to another model.
//! When [`MAX_BOUND_ENV`] models are bound and each has work this pass,
//! another model's index waits for the next pass (which is then due at
//! once), and in that pass may release the bound model served least
//! recently: the bound models are served before another is bound, and a
//! pass binds each slot at most once.
//!
//! Descriptors are read only from what is here: discovery never fetches, so
//! a collection whose descriptor has not arrived is passed over until it
//! has.
//!
//! What it does not do: a descriptor that is registered but named by no
//! record is an ordinary blob, which no store indexes by what it describes,
//! so such an index is not found until its first leaf exists.
//!
//! Which indexes a host maintains is its own choice, as its merges and
//! attachments are: only those whose descriptor names a listed model
//! ([`Config::models`]; the daemon reads [`MODELS_ENV`]). A declaration can
//! never be withdrawn from a grow-only store, so every retired model stays
//! declared; a positive list keeps a retired model out without anyone having
//! to remember it, and a new index of a listed model is still picked up on
//! the next pass. Nothing coordinates two maintainers on one host: if the
//! daemon and an application's Driver both list a model, both bind it (two
//! copies of its weights) and both derive its indexes (the same leaves,
//! twice).

use super::{Derived, Session, Target};
use crate::maintenance::Sink;
use crate::storage::underived;
use anyhow::{Context, Result};
use ed25519_dalek::SigningKey;
use std::ffi::OsStr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use triblespace::core::blob::encodings::simplearchive::SimpleArchive;
use triblespace::core::collection::{
    Collection, CollectionHandle, CollectionRealizationError, Maintenance, Mappings,
};
use triblespace::core::repo::async_store::AsyncBlobStoreAcquire;
use triblespace::core::repo::{SnapshotSource, Store, StoreRead};
use triblespace::prelude::*;
use triblespace_search::semantic_wemm::{
    collection_hex, declared_indexes, WemmIdentity, WemmProfile, WEMM_CONTENT_WINDOWS_TO_NVFP4,
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
            "maintain: every WeMM index the pile's records name whose descriptor names one of {} ({MODELS_ENV}), each with that model; at most {} bound ({MAX_BOUND_ENV}), a failed bind retried after {} s ({BIND_RETRY_ENV}); nothing coordinates another maintainer on this host (an application's Driver) that lists the same model, which binds it too",
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
pub trait Binder: Send {
    /// A bound model, shared by the registration and the maintenance it runs.
    type Model: Clone + Send + Sync + 'static;
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
    /// Maintain the index `target`, which lacks `backlog` foundations, with
    /// `model`: the future derives them.
    fn maintain<'a, S>(
        &self,
        model: &Self::Model,
        pile: &'a mut S,
        target: Target,
        backlog: usize,
        signer: &'a SigningKey,
    ) -> Maintenance<'a, S>
    where
        S: Store + AsyncBlobStoreAcquire + Send;
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
    type Model = Arc<Session>;

    fn bind(&mut self, identity: WemmIdentity) -> Result<Arc<Session>> {
        let session =
            Session::bind_identity(&self.pile, identity, self.residence, self.copy_min_free)?;
        session.start_probe_from_env()?;
        Ok(Arc::new(session))
    }

    fn keeps_every_bind(&self) -> bool {
        self.residence == mary::nn::cuda_bf16_alias::WeightResidence::PileAliased
    }

    fn maintain<'a, S>(
        &self,
        model: &Arc<Session>,
        pile: &'a mut S,
        target: Target,
        backlog: usize,
        signer: &'a SigningKey,
    ) -> Maintenance<'a, S>
    where
        S: Store + AsyncBlobStoreAcquire + Send,
    {
        Session::maintain_in(model, pile, target, backlog, signer)
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

/// A sort key for one model: a pass groups indexes by it.
fn key(identity: &WemmIdentity) -> ([u8; 32], Id, Id) {
    (
        identity.model_collection.raw,
        identity.model_root,
        identity.kernel_profile,
    )
}

fn resolution(what: &str, error: impl std::fmt::Display) -> CollectionRealizationError {
    CollectionRealizationError::Resolution(format!("{what}: {error:#}"))
}

/// One index this pass found of a listed model: its handle, its source, and
/// its backlog, `None` when that could not be read (the line says so).
type Member = (CollectionHandle, CollectionHandle, Option<usize>);

struct Bound<M> {
    identity: WemmIdentity,
    model: M,
    /// The pass that last maintained with it.
    served: u64,
}

/// What the registration keeps between passes: the models it holds bound,
/// the binds that failed and when, and which models it has named. A few
/// entries per model; the indexes themselves are only this pass's.
struct State<B: Binder> {
    binder: B,
    bound: Vec<Bound<B::Model>>,
    failed: Vec<(WemmIdentity, Instant)>,
    named: Vec<WemmIdentity>,
    /// The first model this process bound, for a binder that keeps every
    /// bind ([`Binder::keeps_every_bind`]).
    first_bound: Option<WemmIdentity>,
    passes: u64,
    /// This pass: each listed model's indexes, and the models with work.
    groups: Vec<(WemmIdentity, Vec<Member>)>,
    wanted: Vec<WemmIdentity>,
    /// This pass: the models whose indexes wait for the next one.
    waiting: Vec<WemmIdentity>,
    /// This pass: the models that waited in the last one, which may release
    /// a bound model with work, and how many binds the pass has made.
    entitled: Vec<WemmIdentity>,
    binds: usize,
}

/// The WeMM maintenance registration: runs the WeMM algorithm for every
/// index of a listed model, binding each model on demand through `B` and
/// keeping it bound ([`Config`]: which models, how many at once, when a
/// failed bind is tried again). Its lines go to the sink it is built with.
pub struct Registration<B: Binder = SessionBinder> {
    config: Config,
    sink: Sink,
    state: Mutex<State<B>>,
}

impl Registration<SessionBinder> {
    /// The registration `faculties maintain --declared` runs: the models
    /// and limits of the environment ([`Config::from_env`]), bound from the
    /// pile at `pile` as the environment says ([`SessionBinder::from_env`]).
    pub fn from_env(pile: &std::path::Path, sink: Sink) -> Result<Self> {
        Ok(Self::new(
            Config::from_env()?,
            SessionBinder::from_env(pile)?,
            sink,
        ))
    }
}

impl<B: Binder> Registration<B> {
    pub fn new(config: Config, binder: B, sink: Sink) -> Self {
        Self {
            config,
            sink,
            state: Mutex::new(State {
                binder,
                bound: Vec::new(),
                failed: Vec::new(),
                named: Vec::new(),
                first_bound: None,
                passes: 0,
                groups: Vec::new(),
                wanted: Vec::new(),
                waiting: Vec::new(),
                entitled: Vec::new(),
                binds: 0,
            }),
        }
    }

    fn log(&self, line: String) {
        (self.sink)(&line);
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State<B>> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The models bound now.
    pub fn bound(&self) -> Vec<WemmIdentity> {
        self.state()
            .bound
            .iter()
            .map(|bound| bound.identity)
            .collect()
    }

    /// The interval between two attempts to bind one model, from the end of
    /// the first: the configured one, never under [`MIN_BIND_RETRY`].
    fn retry(&self) -> Duration {
        self.config.retry.max(MIN_BIND_RETRY)
    }

    /// Bind `identity` for `index` if it is not bound, or say why not.
    /// `None` when it cannot be bound now, with a line saying why the first
    /// time.
    fn ensure_bound(&self, state: &mut State<B>, identity: WemmIdentity) -> Option<usize> {
        if let Some(slot) = state.bound.iter().position(|b| b.identity == identity) {
            return Some(slot);
        }
        let members: Vec<Member> = state
            .groups
            .iter()
            .find(|(model, _)| *model == identity)
            .map(|(_, members)| members.clone())
            .unwrap_or_default();
        let now = Instant::now();
        if let Some(position) = state
            .failed
            .iter()
            .position(|(failed, _)| *failed == identity)
        {
            if now.duration_since(state.failed[position].1) < self.retry() {
                return None;
            }
            state.failed.remove(position);
        }
        if let Some(first) = state.first_bound {
            if first != identity && state.binder.keeps_every_bind() {
                // Once per retry interval, like a failed bind.
                self.log(format!(
                    "maintain: not binding model {}: this process bound {} with weights aliased from the pile, whose registrations stay until it exits, so it binds no other model; set WEMM_COPY_WEIGHTS=1 for several models",
                    model_name(&identity),
                    model_name(&first)
                ));
                state.failed.push((identity, now));
                return None;
            }
        }
        if state.bound.len() >= self.config.max_bound {
            // A bound model with nothing to derive this pass goes first. A
            // model that waited last pass may then release the one served
            // least recently while this pass has bound fewer models than
            // there are slots; any other waits for the next pass. So a pass
            // binds at most once per slot, and the bound models are served
            // before another is bound.
            let release = state
                .bound
                .iter()
                .position(|b| !state.wanted.contains(&b.identity))
                .or_else(|| {
                    state
                        .entitled
                        .contains(&identity)
                        .then_some(())
                        .filter(|()| state.binds < self.config.max_bound)
                        .and_then(|()| {
                            state
                                .bound
                                .iter()
                                .enumerate()
                                .min_by_key(|(_, b)| b.served)
                                .map(|(slot, _)| slot)
                        })
                });
            let Some(release) = release else {
                if !state.waiting.contains(&identity) {
                    state.waiting.push(identity);
                    self.log(format!(
                        "maintain: model {} waits for the next pass: the {} bound model(s) have work this one ({MAX_BOUND_ENV}={})",
                        model_name(&identity),
                        state.bound.len(),
                        self.config.max_bound
                    ));
                }
                return None;
            };
            let released = state.bound.remove(release);
            self.log(format!(
                "maintain: releasing model {} to bind {} ({} model(s) have work, {MAX_BOUND_ENV}={})",
                model_name(&released.identity),
                model_name(&identity),
                state.wanted.len(),
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
        match state.binder.bind(identity) {
            Ok(model) => {
                state.first_bound.get_or_insert(identity);
                state.binds += 1;
                self.log(format!(
                    "maintain: bound model {} for index(es) {}",
                    model_name(&identity),
                    names(&members)
                ));
                state.bound.push(Bound {
                    identity,
                    model,
                    served: 0,
                });
                Some(state.bound.len() - 1)
            }
            Err(error) => {
                // From when the attempt ended: a bind slower than the
                // interval is still due again an interval later.
                state.failed.push((identity, Instant::now()));
                self.log(format!(
                    "maintain: skip model {}: {error:#}; index(es) {} not maintained, bind retried after {} s",
                    model_name(&identity),
                    names(&members),
                    self.retry().as_secs()
                ));
                None
            }
        }
    }

    /// The model and the member this pass found for `index`.
    fn found(state: &State<B>, index: CollectionHandle) -> Option<(WemmIdentity, Member)> {
        state.groups.iter().find_map(|(identity, members)| {
            members
                .iter()
                .find(|(member, _, _)| *member == index)
                .map(|member| (*identity, *member))
        })
    }
}

fn names(members: &[Member]) -> String {
    members
        .iter()
        .map(|(index, _, _)| short(*index))
        .collect::<Vec<_>>()
        .join(",")
}

impl<B: Binder> Mappings for Registration<B> {
    fn runs(&self, algorithm: Id) -> bool {
        algorithm == WEMM_CONTENT_WINDOWS_TO_NVFP4
    }

    /// Find this pass's indexes ([`declared_indexes`]), group them by model
    /// with their backlogs, and name each model the first time it is found.
    fn begin_pass<R: StoreRead>(&self, snapshot: &R) {
        let mut state = self.state();
        let state = &mut *state;
        state.passes += 1;
        state.groups.clear();
        state.wanted.clear();
        state.entitled = std::mem::take(&mut state.waiting);
        state.binds = 0;
        let mut declared = match declared_indexes(snapshot) {
            Ok(declared) => declared,
            Err(error) => {
                self.log(format!(
                    "maintain: find the declared WeMM indexes: {error:?}"
                ));
                return;
            }
        };
        declared.sort_by_key(|index| (key(&index.identity), index.index));
        let mut unlisted: Vec<(WemmIdentity, Vec<Member>)> = Vec::new();
        for index in declared {
            let listed = self.config.lists(&index.identity);
            let backlog = if listed {
                let backlog = Collection::<SimpleArchive>::open(snapshot, index.source)
                    .map_err(|e| anyhow::anyhow!("open the index's source: {e}"))
                    .and_then(|source| {
                        let target = Target::open(snapshot, index.index)
                            .map_err(|e| anyhow::anyhow!("open the index: {e}"))?;
                        underived(snapshot, source, target)
                    });
                match backlog {
                    Ok(backlog) => Some(backlog),
                    Err(error) => {
                        self.log(format!(
                            "maintain: index {} (model {}) not read: {error:#}",
                            short(index.index),
                            model_name(&index.identity)
                        ));
                        None
                    }
                }
            } else {
                None
            };
            let groups = if listed {
                &mut state.groups
            } else {
                &mut unlisted
            };
            let member = (index.index, index.source, backlog);
            match groups.last_mut() {
                Some((model, members)) if *model == index.identity => members.push(member),
                _ => groups.push((index.identity, vec![member])),
            }
        }
        // Each model is named once, when it is first found: at the first
        // pass every model then declared, later only new ones.
        for (identity, members) in &unlisted {
            if !state.named.contains(identity) {
                state.named.push(*identity);
                self.log(format!(
                    "maintain: model {} is not listed ({MODELS_ENV}); its index(es) {} are not maintained here",
                    model_name(identity),
                    names(members)
                ));
            }
        }
        for (identity, members) in &state.groups {
            if !state.named.contains(identity) {
                state.named.push(*identity);
                self.log(format!(
                    "maintain: model {} is listed; maintaining its index(es) {}",
                    model_name(identity),
                    names(members)
                ));
            }
            if members
                .iter()
                .any(|(_, _, backlog)| backlog.unwrap_or(0) > 0)
            {
                state.wanted.push(*identity);
            }
        }
    }

    /// The backlog this pass found; none for an index of a model not
    /// listed, one that could not be read (its line is out), or one the
    /// pass did not find.
    fn pending<R: StoreRead>(
        &self,
        _snapshot: &R,
        handle: CollectionHandle,
        _algorithm: Id,
    ) -> Result<usize, CollectionRealizationError> {
        Ok(Self::found(&self.state(), handle)
            .and_then(|(_, (_, _, backlog))| backlog)
            .unwrap_or(0))
    }

    /// Bind the index's model if it has work and is not bound (or say why
    /// not), then derive its backlog with it.
    fn maintain<'a, S>(
        &'a self,
        pile: &'a mut S,
        snapshot: &S::Snapshot,
        handle: CollectionHandle,
        _algorithm: Id,
        signer: &'a SigningKey,
    ) -> Maintenance<'a, S>
    where
        S: Store + AsyncBlobStoreAcquire + Send,
    {
        let opened = |source| -> Result<_, CollectionRealizationError> {
            let source = Collection::<SimpleArchive>::open(snapshot, source)
                .map_err(|e| resolution("open the index's source", e))?;
            let target =
                Target::open(snapshot, handle).map_err(|e| resolution("open the index", e))?;
            Ok((source, target))
        };
        let work = {
            let mut state = self.state();
            match Self::found(&state, handle) {
                Some((identity, (_, source, Some(backlog)))) if backlog > 0 => {
                    self.ensure_bound(&mut state, identity).map(|slot| {
                        state.bound[slot].served = state.passes;
                        (
                            identity,
                            opened(source),
                            backlog,
                            state.bound[slot].model.clone(),
                        )
                    })
                }
                _ => None,
            }
        };
        Box::pin(async move {
            let Some((identity, opened, backlog, model)) = work else {
                return pile
                    .snapshot()
                    .map_err(|e| resolution("observe the index", e));
            };
            let started = Instant::now();
            let (source, target) = opened?;
            let future = self
                .state()
                .binder
                .maintain(&model, &mut *pile, target, backlog, signer);
            let result = future.await;
            let after = pile
                .snapshot()
                .map_err(|e| resolution("observe the index", e))?;
            let derived = Derived::observe(&after, source, target, backlog, started)
                .map_err(|e| resolution("count the index's backlog", e))?;
            match result {
                Ok(_) if derived.derived > 0 || derived.missing > 0 => self.log(format!(
                    "maintain: {derived} (model {})",
                    model_name(&identity)
                )),
                Ok(_) => {}
                Err(error) => self.log(format!(
                    "maintain: index {} (model {}) failed: {error}",
                    short(handle),
                    model_name(&identity)
                )),
            }
            Ok(after)
        })
    }

    /// The next attempt of every failed model that still has work, also one
    /// already due (it came while later models were maintained): the daemon
    /// then passes again at once, and that pass makes the attempt and pushes
    /// its next one forward by the interval. A model that waits for the next
    /// pass is due now.
    fn due(&self) -> Option<Instant> {
        let state = self.state();
        let retry = self.retry();
        let failed = state
            .failed
            .iter()
            .filter(|(identity, _)| state.wanted.contains(identity))
            .map(|(_, ended)| *ended + retry)
            .min();
        if state.waiting.is_empty() {
            failed
        } else {
            Some(failed.map_or_else(Instant::now, |failed| failed.min(Instant::now())))
        }
    }
}

#[cfg(test)]
mod tests;
