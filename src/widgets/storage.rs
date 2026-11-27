//! Immutable dataset access shared by faculty widgets.
//!
//! Widgets consume borrowed [`DatasetView`] values through a keyed
//! [`WidgetContext`]. `StorageState` owns the currently loaded snapshot and the
//! top-bar path selector. Every source is observed through a maintained
//! Rank9-accelerated Succinct view derived from its fixed V4 descriptor-handle
//! collection under the pile's durable
//! signer. Repository branches, mutable heads, and compatibility fallbacks do
//! not participate in this boundary. The interactive viewer loads all sources;
//! focused capture binaries request only their source dependency
//! closure. Loading ensures roots and maintains each immediate derivation.
//! One common store snapshot attaches facts and positive latest/LWW indexes
//! independently; those unsigned artifacts are cache exhaust, not
//! authoritative writes. Most sources are
//! fixed descriptor-handle collections. Secrets uses the same explicit
//! collection configuration and preserves local delivered-envelope possession
//! semantics; collection READ governs replication, not opening a local wrap.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::SystemTime;

use triblespace::core::blob::encodings::succinctarchive::{
    Rank9AcceleratedSuccinctArchiveBlob, SuccinctArchiveBlob,
};
use triblespace::core::collection::latest::{LatestBlob, LatestIndex};
use triblespace::core::collection::lww_register::{LwwIndex, LwwQuery, LwwRegisterBlob};
use triblespace::core::collection::{
    observed_store::DependencyTracker, AttachedSnapshot, Collection, CollectionEncoding,
    CollectionHandle, CollectionSnapshotExt, CollectionStoreExt, Cover, Support,
};
use triblespace::core::repo::pile::Pile;
use triblespace::core::repo::{BlobStoreGet, CapabilityProofRead, ObservedStore, SnapshotSource};
use triblespace::prelude::Id;
use GORBIE::prelude::CardCtx;

use crate::collection_names::open_configured;
use crate::schemas::atlas::DEFAULT_SCOPE_ID as SCHEMAS_SCOPE_ID;
use crate::schemas::blockdag::DEFAULT_SCOPE_ID as ARCHIVE_SCOPE_ID;
use crate::schemas::cognition::DEFAULT_SCOPE_ID as COGNITION_SCOPE_ID;
use crate::schemas::compass::DEFAULT_SCOPE_ID as GOALS_SCOPE_ID;
use crate::schemas::decide::DEFAULT_SCOPE_ID as DECISIONS_SCOPE_ID;
use crate::schemas::discord::DEFAULT_SCOPE_ID as DISCORD_SCOPE_ID;
use crate::schemas::files::DEFAULT_SCOPE_ID as FILES_SCOPE_ID;
use crate::schemas::headspace::DEFAULT_SCOPE_ID as PROFILES_SCOPE_ID;
use crate::schemas::mail::DEFAULT_SCOPE_ID as MAIL_SCOPE_ID;
use crate::schemas::memory::DEFAULT_SCOPE_ID as MEMORY_SCOPE_ID;
use crate::schemas::message::DEFAULT_SCOPE_ID as MESSAGES_SCOPE_ID;
use crate::schemas::planner::DEFAULT_SCOPE_ID as EVENTS_SCOPE_ID;
use crate::schemas::quota::DEFAULT_SCOPE_ID as QUOTA_SCOPE_ID;
use crate::schemas::relations::DEFAULT_SCOPE_ID as RELATIONS_SCOPE_ID;
use crate::schemas::rules::DEFAULT_SCOPE_ID as RULES_SCOPE_ID;
use crate::schemas::spawn::DEFAULT_SCOPE_ID as SPAWN_SCOPE_ID;
use crate::schemas::teams::DEFAULT_SCOPE_ID as TEAMS_SCOPE_ID;
use crate::schemas::wiki::DEFAULT_SCOPE_ID as KNOWLEDGE_SCOPE_ID;
use crate::secrets::{storage as secret_storage, SecretsSnapshot};
use crate::storage::{
    open_secrets_collection_read, AcquiringReader, CollectionRoutes, FactArchive, FacultyStore,
    Storage,
};

mod reader;
pub use reader::WidgetReader;
type PileSnapshot = WidgetReader;

/// Stable logical input requested by a widget.
///
/// These keys describe consumers, not storage layout. Several consumers may
/// intentionally share one exact collection (Reason and Diagnostics both observe
/// Cognition), while the descriptor identity remains hidden behind this
/// boundary.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum SourceKey {
    Archive,
    Schemas,
    Goals,
    Decisions,
    Discord,
    Files,
    Profiles,
    Mail,
    Memory,
    Messages,
    Events,
    Reason,
    Relations,
    Rules,
    Quota,
    Secrets,
    Spawn,
    Teams,
    Diagnostics,
    Knowledge,
}

impl SourceKey {
    /// Every logical input understood by the viewer core.
    pub const ALL: [Self; 20] = [
        Self::Archive,
        Self::Schemas,
        Self::Goals,
        Self::Decisions,
        Self::Discord,
        Self::Files,
        Self::Profiles,
        Self::Mail,
        Self::Memory,
        Self::Messages,
        Self::Events,
        Self::Reason,
        Self::Relations,
        Self::Rules,
        Self::Quota,
        Self::Secrets,
        Self::Spawn,
        Self::Teams,
        Self::Diagnostics,
        Self::Knowledge,
    ];

    /// Logical inputs that must accompany this source for rendering. Storage
    /// layout stays out of this relation: the transitive closure is computed
    /// over semantic source keys and scopes are deduplicated only afterwards.
    fn dependencies(self) -> &'static [Self] {
        match self {
            Self::Profiles => &[Self::Secrets],
            Self::Mail | Self::Messages | Self::Events => &[Self::Relations],
            Self::Diagnostics => &[
                Self::Profiles,
                Self::Secrets,
                Self::Relations,
                Self::Messages,
            ],
            _ => &[],
        }
    }
}

fn source_closure(sources: impl IntoIterator<Item = SourceKey>) -> BTreeSet<SourceKey> {
    let mut closure = BTreeSet::new();
    let mut pending: Vec<_> = sources.into_iter().collect();
    while let Some(source) = pending.pop() {
        if closure.insert(source) {
            pending.extend_from_slice(source.dependencies());
        }
    }
    closure
}

/// Opaque cache identity for one logical dataset view.
///
/// Widgets compare revisions for equality; the storage backend owns their
/// construction. The digest includes each attached relation's descriptor,
/// its already-attached resident cover, and what the read took beside the
/// cover: the residual foundations and unread support. A raw-only commit
/// moves the token because it adds a known pending gap; raw payload arrival
/// alone does not make that commit visible. A later stored MAP changes the
/// visible cover. It is a widget cache token, not a durable collection
/// record or an authorization proof. Equivalent physical compaction may
/// invalidate a cached projection, but computing the token never resolves
/// the cover's historical support.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DatasetRevision([u8; 32]);

impl DatasetRevision {
    fn hash_collection_cover<E: CollectionEncoding>(
        hasher: &mut blake3::Hasher,
        collection: CollectionHandle,
        cover: &Cover<E>,
    ) {
        hasher.update(&collection.raw);
        for member in cover.members() {
            hasher.update(&member.raw);
        }
        hasher.update(&(cover.len() as u128).to_le_bytes());
    }

    /// Hash what one attached read holds: its cover, its residual, and the
    /// residual foundations it could not read.
    fn hash_attached<R, E>(
        hasher: &mut blake3::Hasher,
        attached: &AttachedSnapshot<R, E>,
        unread: &Support,
    ) where
        R: triblespace::core::repo::StoreSnapshot,
        E: CollectionEncoding,
    {
        let collection = attached.cover().collection().handle();
        let parent = attached.support().collection().handle();
        Self::hash_collection_cover(hasher, collection, attached.cover());
        Self::hash_collection_cover(hasher, parent, attached.residual());
        Self::hash_collection_cover(hasher, parent, unread);
    }

    fn from_attached<R, E>(attached: &AttachedSnapshot<R, E>, unread: &Support) -> Self
    where
        R: triblespace::core::repo::StoreSnapshot,
        E: CollectionEncoding,
    {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"faculties.viewer.dataset-revision.v4");
        Self::hash_attached(&mut hasher, attached, unread);
        Self(*hasher.finalize().as_bytes())
    }

    fn include_attached<R, E>(&mut self, attached: &AttachedSnapshot<R, E>, unread: &Support)
    where
        R: triblespace::core::repo::StoreSnapshot,
        E: CollectionEncoding,
    {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"faculties.viewer.dataset-relation.v2");
        hasher.update(&self.0);
        Self::hash_attached(&mut hasher, attached, unread);
        self.0 = *hasher.finalize().as_bytes();
    }

    fn from_secrets(snapshot: &SecretsSnapshot<PileSnapshot>) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"faculties.viewer.secrets-revision.v3");
        Self::hash_collection_cover(&mut hasher, snapshot.collection(), snapshot.support());
        Self(*hasher.finalize().as_bytes())
    }
}

/// Borrowed immutable input for one widget dataset.
#[derive(Clone, Copy)]
pub struct DatasetView<'a> {
    pub facts: &'a FactArchive,
    pub reader: &'a PileSnapshot,
    pub revision: DatasetRevision,
    /// What this dataset's read could not load
    /// ([`crate::storage::unread_foundations_note`]): `facts` omits whatever
    /// those foundations hold, so an empty answer over them is not "none".
    pub notes: &'a [String],
    lww_registers: &'a BTreeMap<(Id, Id), LwwQuery>,
    latest_indexes: &'a BTreeMap<Id, LatestIndex>,
}

impl DatasetView<'_> {
    /// Maintained LWW order for the requested identity and order attributes.
    pub fn lww_register(&self, identity: Id, orders: Id) -> Option<&LwwQuery> {
        self.lww_registers.get(&(identity, orders))
    }

    /// Known latest states for the requested supersession edge attribute.
    pub fn latest_index(&self, observes: Id) -> Option<&LatestIndex> {
        self.latest_indexes.get(&observes)
    }
}

/// Keyed borrowed inputs for one viewer render.
#[derive(Clone, Copy)]
pub struct WidgetContext<'a> {
    datasets: Option<&'a BTreeMap<SourceKey, LoadedDataset>>,
    routed_but_unavailable: Option<&'a BTreeSet<SourceKey>>,
    secrets: Option<&'a LoadedSecrets>,
}

impl<'a> WidgetContext<'a> {
    /// Return the exact requested logical dataset, or `None` when that source
    /// is absent. Missing sources never shift another source into its place.
    pub fn dataset(&self, key: SourceKey) -> Option<DatasetView<'a>> {
        self.datasets?.get(&key)?.view().ok()
    }

    /// A workspace observation omits absent routes and records routes it failed to read.
    pub(super) fn missing_route(&self, key: SourceKey) -> bool {
        !self.contains(key)
            && self
                .routed_but_unavailable
                .is_some_and(|keys| !keys.contains(&key))
    }

    /// The selected observation token is available before decoding a deferred archive.
    pub(super) fn revision(&self, key: SourceKey) -> Option<DatasetRevision> {
        self.datasets?.get(&key).map(|dataset| dataset.revision)
    }

    /// What the read behind `key` could not load; empty for a complete read
    /// or an absent source.
    pub fn notes(&self, key: SourceKey) -> &'a [String] {
        self.datasets
            .and_then(|datasets| datasets.get(&key))
            .map_or(&[], |dataset| dataset.notes.as_slice())
    }

    /// Keep this frozen input on an asynchronous card worker. Archives, snapshots
    /// and prepared indexes share their underlying observation.
    pub(super) fn owned_dataset(&self, key: SourceKey) -> Option<LoadedDataset> {
        self.datasets?.get(&key).cloned()
    }

    /// Return the explicitly configured readable Secrets collection.
    pub fn secrets(&self) -> Option<SecretsView<'a>> {
        self.secrets.map(LoadedSecrets::view)
    }

    /// Whether this semantic source is present in the loaded closure.
    pub fn contains(&self, key: SourceKey) -> bool {
        match key {
            SourceKey::Secrets => self.secrets.is_some(),
            _ => self
                .datasets
                .is_some_and(|datasets| datasets.contains_key(&key)),
        }
    }
}

/// Borrowed Secrets snapshot plus its viewer cache token.
#[derive(Clone, Copy)]
pub struct SecretsView<'a> {
    pub snapshot: &'a SecretsSnapshot<PileSnapshot>,
    pub revision: DatasetRevision,
}

#[derive(Clone)]
pub(super) struct LoadedDataset {
    facts: DatasetFacts,
    reader: PileSnapshot,
    revision: DatasetRevision,
    notes: Arc<Vec<String>>,
    lww_registers: Arc<BTreeMap<(Id, Id), LwwQuery>>,
    latest_indexes: Arc<BTreeMap<Id, LatestIndex>>,
}

#[derive(Clone)]
enum DatasetFacts {
    Ready(FactArchive),
    Deferred(Arc<DeferredFacts>),
}

/// Freeze the native selection on the observation owner. Decode its archival
/// payload only when a consumer requests facts, on Colony's background worker.
struct DeferredFacts {
    selected: AttachedSnapshot<WidgetReader, Rank9AcceleratedSuccinctArchiveBlob>,
    facts: OnceLock<Result<FactArchive, String>>,
}

impl LoadedDataset {
    fn new(
        facts: FactArchive,
        revision: DatasetRevision,
        notes: Arc<Vec<String>>,
        reader: PileSnapshot,
        lww_registers: BTreeMap<(Id, Id), LwwQuery>,
        latest_indexes: BTreeMap<Id, LatestIndex>,
    ) -> Self {
        Self {
            facts: DatasetFacts::Ready(facts),
            reader,
            revision,
            notes,
            lww_registers: Arc::new(lww_registers),
            latest_indexes: Arc::new(latest_indexes),
        }
    }

    fn deferred_spawn(
        reader: WidgetReader,
        selected: AttachedSnapshot<WidgetReader, Rank9AcceleratedSuccinctArchiveBlob>,
    ) -> Self {
        // A usable archive has no unread residual. An incomplete read below
        // returns an error and can never expose a partial dataset at this token:
        // the token is fixed before the archive is decoded, so it could not
        // tell a partial read from the complete one of the same selection.
        let empty = selected
            .residual()
            .difference(selected.residual())
            .expect("one frozen residual has one collection");
        let revision = DatasetRevision::from_attached(&selected, &empty);
        Self {
            facts: DatasetFacts::Deferred(Arc::new(DeferredFacts {
                selected,
                facts: OnceLock::new(),
            })),
            reader,
            revision,
            notes: Default::default(),
            lww_registers: Default::default(),
            latest_indexes: Default::default(),
        }
    }

    /// [`Self::view`] only when its read loaded every foundation, for a
    /// widget that states a property of the whole collection (a rule
    /// program's termination, say), which a partial read would make false.
    /// Otherwise the error names what could not be loaded.
    pub(super) fn complete_view(&self) -> Result<DatasetView<'_>, String> {
        if !self.notes.is_empty() {
            return Err(self.notes.join("; "));
        }
        self.view().map_err(str::to_owned)
    }

    pub(super) fn view(&self) -> Result<DatasetView<'_>, &str> {
        let facts = match &self.facts {
            DatasetFacts::Ready(facts) => facts,
            DatasetFacts::Deferred(deferred) => deferred
                .facts
                .get_or_init(|| {
                    triblespace::core::collection::succinctarchive_union::read_attached_acquiring(
                        &deferred.selected,
                    )
                    .map_err(anyhow::Error::from)
                    .and_then(crate::storage::require_complete_attached_read)
                    .map_err(|error| format!("Spawn evidence unavailable: {error:#}"))
                })
                .as_ref()
                .map_err(String::as_str)?,
        };
        Ok(DatasetView {
            facts,
            reader: &self.reader,
            revision: self.revision,
            notes: &self.notes,
            lww_registers: &self.lww_registers,
            latest_indexes: &self.latest_indexes,
        })
    }
}

struct LoadedSecrets {
    snapshot: SecretsSnapshot<PileSnapshot>,
    revision: DatasetRevision,
}

struct LoadedInputs {
    unavailable: BTreeSet<SourceKey>,
    datasets: BTreeMap<SourceKey, LoadedDataset>,
    secrets: Option<LoadedSecrets>,
}

/// A host-owned frozen observation. It owns native views, never a pile, peer,
/// path-based Storage, or a second catalogue of faculty entities.
pub struct WorkspaceObservation(LoadedInputs);

impl WorkspaceObservation {
    /// A local presentation-cache token, never a stored id or authorization
    /// generation. Payload-only arrivals can change a rendered answer without
    /// changing its collection cover; keep navigation while replacing caches.
    pub fn with_presentation_revision(mut self, serial: u64) -> Self {
        let advance = |revision: &mut DatasetRevision| {
            let mut hasher = blake3::Hasher::new();
            hasher.update(&revision.0);
            hasher.update(&serial.to_le_bytes());
            *revision = DatasetRevision(*hasher.finalize().as_bytes());
        };
        for dataset in self.0.datasets.values_mut() {
            advance(&mut dataset.revision);
        }
        if let Some(secrets) = &mut self.0.secrets {
            advance(&mut secrets.revision);
        }
        self
    }

    pub fn loaded_context(&self) -> WidgetContext<'_> {
        WidgetContext {
            datasets: Some(&self.0.datasets),
            secrets: self.0.secrets.as_ref(),
            routed_but_unavailable: Some(&self.0.unavailable),
        }
    }

    /// What each loaded dataset's read could not load: those views show what
    /// is here, and an empty one is not known to be empty. A host shows these
    /// beside the views, as it shows [`Self::unavailable_labels`].
    pub fn partial_notes(&self) -> impl Iterator<Item = &str> + '_ {
        partial_notes(&self.0.datasets)
    }

    pub fn unavailable_labels(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.0.unavailable.iter().map(|key| {
            COLLECTION_SOURCE_CATALOG
                .iter()
                .find(|entry| entry.key == *key)
                .map(|entry| entry.label)
                .unwrap_or("Secrets")
        })
    }

    /// Borrow the host's existing pile for one explicit workspace observation.
    /// The caller owns synchronization, cancellation and workspace revalidation.
    /// No path is accepted and no storage owner or transport is constructed.
    pub async fn observe(
        pile: &mut Pile,
        signer: &ed25519_dalek::SigningKey,
        routes: &CollectionRoutes,
    ) -> Result<Self, String> {
        Self::observe_tracked(pile, signer, routes, Default::default()).await
    }

    /// The same raw read-set follows preparation and lazy widget reads, including
    /// misses. The host compares it against a fixed native snapshot baseline.
    pub async fn observe_tracked(
        pile: &mut Pile,
        signer: &ed25519_dalek::SigningKey,
        routes: &CollectionRoutes,
        tracker: DependencyTracker,
    ) -> Result<Self, String> {
        let mut observed = ObservedStore::with_tracker(pile, tracker);
        let pile = &mut observed;
        let mut sources = source_closure(SourceKey::ALL);
        retain_routed_sources(&mut sources, routes);
        let mut unavailable = BTreeSet::new();
        let admitted = pile.snapshot().map_err(|error| error.to_string())?;
        for (scope, _) in collection_scopes(&sources) {
            if crate::collection_names::open_workspace_read_in(
                &admitted,
                signer.verifying_key(),
                routes[&scope],
            )
            .is_err()
            {
                unavailable.extend(remove_scope(&mut sources, scope));
            }
        }
        let prepared = prepare_inputs(pile, &sources, true, unavailable, |pile, scope| {
            crate::collection_names::open_workspace_in(&pile.snapshot()?, routes[&scope])
        })?;
        let secrets = if sources.contains(&SourceKey::Secrets) {
            let observed = async {
                // READ controls replication, not possession of delivered wraps.
                let source = crate::collection_names::open_workspace_in(
                    &pile.snapshot()?,
                    routes[&crate::secrets::DEFAULT_SCOPE_ID],
                )?;
                let collection = secret_storage::SecretsCollection::from_source(pile, source)?;
                let snapshot =
                    secret_storage::ensure_and_snapshot(pile, collection, signer).await?;
                Ok::<_, anyhow::Error>((collection, snapshot))
            }
            .await;
            Some(observed)
        } else {
            None
        };
        let mut prepared = prepared;
        let (snapshot, secrets) = match secrets {
            Some(Ok((collection, secrets))) => (secrets.store_snapshot().clone(), Some(collection)),
            other => {
                if other.is_some() {
                    prepared.unavailable.insert(SourceKey::Secrets);
                }
                (pile.snapshot().map_err(|error| error.to_string())?, None)
            }
        };
        read_inputs(WidgetReader::Resident(snapshot), secrets, prepared, true).map(Self)
    }
}

impl LoadedSecrets {
    fn new(snapshot: SecretsSnapshot<PileSnapshot>) -> Self {
        let revision = DatasetRevision::from_secrets(&snapshot);
        Self { snapshot, revision }
    }

    fn view(&self) -> SecretsView<'_> {
        SecretsView {
            snapshot: &self.snapshot,
            revision: self.revision,
        }
    }
}

#[derive(Clone, Copy)]
struct CollectionSource {
    key: SourceKey,
    scope: Id,
    label: &'static str,
}

// Deliberately private: a widget asks for a semantic source key, not a scope,
// descriptor, branch, or migration epoch. The descriptor handle is derived
// canonically from each fixed scope when the pile is loaded.
const COLLECTION_SOURCE_CATALOG: &[CollectionSource] = &[
    CollectionSource {
        key: SourceKey::Archive,
        scope: ARCHIVE_SCOPE_ID,
        label: "Archive",
    },
    CollectionSource {
        key: SourceKey::Schemas,
        scope: SCHEMAS_SCOPE_ID,
        label: "Schemas",
    },
    CollectionSource {
        key: SourceKey::Goals,
        scope: GOALS_SCOPE_ID,
        label: "Goals",
    },
    CollectionSource {
        key: SourceKey::Decisions,
        scope: DECISIONS_SCOPE_ID,
        label: "Decisions",
    },
    CollectionSource {
        key: SourceKey::Discord,
        scope: DISCORD_SCOPE_ID,
        label: "Discord",
    },
    CollectionSource {
        key: SourceKey::Files,
        scope: FILES_SCOPE_ID,
        label: "Files",
    },
    CollectionSource {
        key: SourceKey::Profiles,
        scope: PROFILES_SCOPE_ID,
        label: "Profiles",
    },
    CollectionSource {
        key: SourceKey::Mail,
        scope: MAIL_SCOPE_ID,
        label: "Mail",
    },
    CollectionSource {
        key: SourceKey::Memory,
        scope: MEMORY_SCOPE_ID,
        label: "Memory",
    },
    CollectionSource {
        key: SourceKey::Messages,
        scope: MESSAGES_SCOPE_ID,
        label: "Messages",
    },
    CollectionSource {
        key: SourceKey::Events,
        scope: EVENTS_SCOPE_ID,
        label: "Events",
    },
    CollectionSource {
        key: SourceKey::Reason,
        scope: COGNITION_SCOPE_ID,
        label: "Cognition",
    },
    CollectionSource {
        key: SourceKey::Relations,
        scope: RELATIONS_SCOPE_ID,
        label: "Relations",
    },
    CollectionSource {
        key: SourceKey::Rules,
        scope: RULES_SCOPE_ID,
        label: "Rules",
    },
    CollectionSource {
        key: SourceKey::Quota,
        scope: QUOTA_SCOPE_ID,
        label: "Quota",
    },
    CollectionSource {
        key: SourceKey::Spawn,
        scope: SPAWN_SCOPE_ID,
        label: "Spawn",
    },
    CollectionSource {
        key: SourceKey::Teams,
        scope: TEAMS_SCOPE_ID,
        label: "Teams",
    },
    CollectionSource {
        key: SourceKey::Diagnostics,
        scope: COGNITION_SCOPE_ID,
        label: "Cognition",
    },
    CollectionSource {
        key: SourceKey::Knowledge,
        scope: KNOWLEDGE_SCOPE_ID,
        label: "Knowledge",
    },
];

fn collection_scopes(sources: &BTreeSet<SourceKey>) -> Vec<(Id, &'static str)> {
    let mut scopes: Vec<_> = COLLECTION_SOURCE_CATALOG
        .iter()
        .filter(|source| sources.contains(&source.key))
        .map(|source| (source.scope, source.label))
        .collect();
    scopes.sort_by_key(|(scope, _)| *scope);
    scopes.dedup_by_key(|(scope, _)| *scope);
    scopes
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileStamp {
    length: u64,
    modified: Option<SystemTime>,
}

/// Shared read-only pile state and top-bar path selector.
pub struct StorageState {
    workspace_only: bool,
    unavailable: BTreeSet<SourceKey>,
    datasets: Option<BTreeMap<SourceKey, LoadedDataset>>,
    secrets: Option<LoadedSecrets>,
    sources: BTreeSet<SourceKey>,
    storage: Storage,
    pile_path_text: String,
    stamp: Option<FileStamp>,
    error: Option<String>,
}

impl StorageState {
    /// Stash a pile path for lazy all-source loading. No I/O happens here,
    /// which keeps eager notebook-state construction cheap.
    pub fn new(pile_path: impl Into<PathBuf>) -> Self {
        Self::with_sources(pile_path, SourceKey::ALL)
    }

    /// Stash a pile path for lazy, source-scoped loading.
    ///
    /// The requested semantic inputs are expanded through their declarative
    /// dependency closure. Only those collection scopes are maintained;
    /// consumers can observe the requested keys and their logical dependencies,
    /// but never unrelated keys that happen to share a scope.
    pub fn for_sources(
        pile_path: impl Into<PathBuf>,
        sources: impl IntoIterator<Item = SourceKey>,
    ) -> Self {
        Self::with_sources(pile_path, sources)
    }

    fn with_sources(
        pile_path: impl Into<PathBuf>,
        sources: impl IntoIterator<Item = SourceKey>,
    ) -> Self {
        Self::with_storage(Storage::new(pile_path.into(), None), sources)
    }

    /// Share an explicit store while retaining operation-local dataset views.
    /// Construction and source dependency selection perform no storage I/O.
    pub fn with_storage(storage: Storage, sources: impl IntoIterator<Item = SourceKey>) -> Self {
        let storage = storage.retained();
        let pile_path_text = storage.path().to_string_lossy().into_owned();
        Self {
            workspace_only: false,
            unavailable: BTreeSet::new(),
            datasets: None,
            secrets: None,
            sources: source_closure(sources),
            storage,
            pile_path_text,
            stamp: None,
            error: None,
        }
    }

    /// Observe only explicitly routed workspace roles. Missing roles are
    /// omitted; denied or unavailable roles cannot hide independent views.
    /// The constructor is I/O-free; refresh belongs on the host's worker.
    pub fn for_workspace(storage: Storage) -> Result<Self, String> {
        if storage.collection_routes().is_none() {
            return Err("Choose a workspace before opening its views.".into());
        }
        let mut state = Self::with_storage(storage, SourceKey::ALL);
        state.workspace_only = true;
        Ok(state)
    }

    /// [`WorkspaceObservation::partial_notes`] for the loaded observation.
    pub fn partial_notes(&self) -> impl Iterator<Item = &str> + '_ {
        self.datasets.iter().flat_map(partial_notes)
    }

    pub fn unavailable_labels(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.unavailable.iter().map(|key| {
            COLLECTION_SOURCE_CATALOG
                .iter()
                .find(|entry| entry.key == *key)
                .map(|entry| entry.label)
                .unwrap_or("Secrets")
        })
    }

    /// Select an explicit launcher-owned signing key for lazy observation.
    /// This constructor performs no I/O. CLI viewers retain the adjacent-key
    /// default by leaving it unset; changing the pile does not change an
    /// explicitly configured key.
    pub fn with_key_path(mut self, key_path: Option<PathBuf>) -> Self {
        if self.storage.key_path() != key_path.as_deref() {
            if self.storage.collection_routes().is_some() {
                self.error = Some("workspace selection is immutable; choose a new configured workspace to change its key".to_owned());
                return self;
            }
            self.storage = Storage::shared(self.storage.path().to_owned(), key_path);
            self.datasets = None;
            self.secrets = None;
            self.stamp = None;
            self.error = None;
        }
        self
    }

    /// Borrow all currently loaded datasets through their logical keys.
    ///
    /// The first call loads lazily. Later calls cheaply compare the pile file
    /// stamp and reload after an external append. A failed live reload retains
    /// the last coherent snapshot and surfaces the error until explicit OPEN.
    pub fn context(&mut self) -> WidgetContext<'_> {
        self.refresh();
        self.loaded_context()
    }

    /// Refresh on a host-owned worker. This may perform storage I/O.
    pub fn refresh(&mut self) {
        self.ensure_loaded();
    }

    /// Borrow the already-loaded immutable observation without refresh or I/O.
    /// Hosts must visibly handle `error()` and schedule `refresh()` themselves.
    pub fn loaded_context(&self) -> WidgetContext<'_> {
        WidgetContext {
            datasets: self.datasets.as_ref(),
            routed_but_unavailable: (self.workspace_only && self.datasets.is_some())
                .then_some(&self.unavailable),
            secrets: self.secrets.as_ref(),
        }
    }

    /// Reopen against `path`. Passing the current path still forces a reload,
    /// which is the behavior of the top-bar OPEN action.
    pub fn set_pile_path(&mut self, path: impl Into<PathBuf>) {
        let path = path.into();
        let changed = path != self.storage.path();
        if changed && self.storage.collection_routes().is_some() {
            self.error = Some("workspace selection is immutable; choose a new configured workspace to change its pile".to_owned());
            return;
        }
        self.pile_path_text = path.to_string_lossy().into_owned();
        self.error = None;
        if changed {
            self.storage = Storage::shared(path, self.storage.key_path().map(Path::to_owned));
            self.datasets = None;
            self.secrets = None;
            self.stamp = None;
        }
        self.reload_current_path();
    }

    fn ensure_loaded(&mut self) {
        if self.datasets.is_none() {
            if self.error.is_none() {
                self.reload_current_path();
            }
            return;
        }
        if self.error.is_some() {
            return;
        }
        match file_stamp(self.storage.path()) {
            Ok(stamp) if Some(stamp) != self.stamp => self.reload_current_path(),
            Ok(_) => {}
            Err(error) => self.error = Some(error),
        }
    }

    fn reload_current_path(&mut self) {
        let result = if self.workspace_only {
            file_stamp(self.storage.path()).and_then(|stamp| {
                load_inputs_mode(&self.storage, &self.sources, true).map(|inputs| (inputs, stamp))
            })
        } else {
            load_consistent_inputs(&self.storage, &self.sources)
        };
        match result {
            Ok((inputs, stamp)) => {
                self.unavailable = inputs.unavailable;
                self.datasets = Some(inputs.datasets);
                self.secrets = inputs.secrets;
                self.stamp = Some(stamp);
                self.error = None;
            }
            Err(error) => {
                self.error = Some(error);
            }
        }
    }

    /// The store this state observes, for host-performed writes. A write
    /// through it moves the pile stamp, so the next `context()` reloads.
    pub fn storage(&self) -> &Storage {
        &self.storage
    }

    /// Current pile load/reload error, if any.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Render the path selector and load status. OPEN always reloads, including
    /// when the entered path is unchanged.
    pub fn top_bar(&mut self, ctx: &mut CardCtx<'_>) {
        self.ensure_loaded();
        let is_open = self.datasets.is_some();
        let has_error = self.error.is_some();
        let mut reopen = false;
        let status_color = if has_error {
            egui::Color32::from_rgb(0xcc, 0x0a, 0x17)
        } else if is_open {
            egui::Color32::from_rgb(0x23, 0x7f, 0x52)
        } else {
            egui::Color32::from_rgb(0x4d, 0x55, 0x59)
        };
        let panel_fill = ctx.ctx().global_style().visuals.panel_fill;
        let bar_bg = egui::Color32::from_rgba_unmultiplied(
            panel_fill.r().saturating_sub(6),
            panel_fill.g().saturating_sub(6),
            panel_fill.b().saturating_sub(6),
            255,
        );
        let muted = egui::Color32::from_rgb(0x8a, 0x8a, 0x8a);
        let ui = ctx.ui_mut();
        egui::Frame::NONE
            .fill(bar_bg)
            .stroke(egui::Stroke::new(1.0, egui::Color32::from_black_alpha(40)))
            .corner_radius(egui::CornerRadius::same(4))
            .inner_margin(egui::Margin::symmetric(10, 6))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    let (dot_rect, _) =
                        ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                    ui.painter()
                        .circle_filled(dot_rect.center(), 4.0, status_color);
                    ui.label(
                        egui::RichText::new("PILE")
                            .small()
                            .monospace()
                            .strong()
                            .color(status_color),
                    );
                    ui.label(egui::RichText::new("│").small().color(muted));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let open_btn = ui.add(
                            egui::Button::new(
                                egui::RichText::new("OPEN").small().monospace().strong(),
                            )
                            .min_size(egui::vec2(52.0, 22.0)),
                        );
                        if open_btn.clicked() {
                            reopen = true;
                        }
                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            let response = ui.add(GORBIE::widgets::TextField::singleline(
                                &mut self.pile_path_text,
                            ));
                            if response.lost_focus()
                                && ui.input(|input| input.key_pressed(egui::Key::Enter))
                            {
                                reopen = true;
                            }
                        });
                    });
                });
            });
        if reopen {
            self.set_pile_path(PathBuf::from(self.pile_path_text.trim()));
        }

        if let Some(error) = self.error.as_ref() {
            render_banner(
                ctx,
                "\u{26a0}",
                &format!("pile load error: {error}"),
                ctx.ctx().global_style().visuals.error_fg_color,
            );
        }
    }
}

/// Each distinct note of the datasets' reads; scopes shared by several
/// sources carry one note.
fn partial_notes(datasets: &BTreeMap<SourceKey, LoadedDataset>) -> impl Iterator<Item = &str> {
    let mut seen = BTreeSet::new();
    datasets
        .values()
        .flat_map(|dataset| dataset.notes.iter())
        .filter(move |note| seen.insert(note.as_str()))
        .map(String::as_str)
}

fn file_stamp(path: &Path) -> Result<FileStamp, String> {
    let metadata = std::fs::metadata(path)
        .map_err(|error| format!("inspect pile {}: {error}", path.display()))?;
    Ok(FileStamp {
        length: metadata.len(),
        modified: metadata.modified().ok(),
    })
}

fn load_consistent_inputs(
    storage: &Storage,
    sources: &BTreeSet<SourceKey>,
) -> Result<(LoadedInputs, FileStamp), String> {
    // All datasets select one store snapshot. Exact body fetches append cache
    // bytes and may change the file stamp, but cannot make that observation
    // incoherent. Keep the pre-read stamp conservatively: any append during
    // the load prompts a later refresh, never a retry/reselection inside it.
    let before = file_stamp(storage.path())?;
    let inputs = load_inputs(storage, sources)?;
    Ok((inputs, before))
}

fn load_inputs(storage: &Storage, sources: &BTreeSet<SourceKey>) -> Result<LoadedInputs, String> {
    load_inputs_mode(storage, sources, false)
}

fn remove_scope(sources: &mut BTreeSet<SourceKey>, scope: Id) -> BTreeSet<SourceKey> {
    let removed: BTreeSet<_> = COLLECTION_SOURCE_CATALOG
        .iter()
        .filter(|entry| entry.scope == scope && sources.contains(&entry.key))
        .map(|entry| entry.key)
        .collect();
    sources.retain(|key| !removed.contains(key));
    removed
}

fn retain_routed_sources(sources: &mut BTreeSet<SourceKey>, routes: &CollectionRoutes) {
    sources.retain(|key| {
        let scope = if *key == SourceKey::Secrets {
            crate::secrets::DEFAULT_SCOPE_ID
        } else {
            COLLECTION_SOURCE_CATALOG
                .iter()
                .find(|entry| entry.key == *key)
                .expect("every source has a role")
                .scope
        };
        routes.contains_key(&scope)
    });
}

fn load_inputs_mode(
    storage: &Storage,
    requested: &BTreeSet<SourceKey>,
    partial: bool,
) -> Result<LoadedInputs, String> {
    storage
        .with_store(|store, signer, runtime| {
            let mut sources = requested.clone();
            let mut unavailable = BTreeSet::new();
            if partial {
                let routes = storage
                    .collection_routes()
                    .ok_or_else(|| anyhow::anyhow!("workspace routes are required"))?;
                retain_routed_sources(&mut sources, routes);
            }
            for (scope, _) in collection_scopes(&sources) {
                if let Err(error) =
                    storage.open_collection_read(store, scope, signer.verifying_key(), runtime)
                {
                    if !partial {
                        return Err(error);
                    }
                    unavailable.extend(remove_scope(&mut sources, scope));
                }
            }
            if sources.contains(&SourceKey::Secrets) {
                let admission = (|| {
                    // Secrets READ governs replication. Opening an already
                    // delivered local envelope keeps Secrets' possession rules.
                    // The explicit workspace route is still required above.
                    storage.open_secrets_collection(
                        store,
                        signer.verifying_key(),
                        runtime,
                        false,
                    )?;
                    Ok::<_, anyhow::Error>(())
                })();
                if let Err(error) = admission {
                    if !partial {
                        return Err(error);
                    }
                    sources.remove(&SourceKey::Secrets);
                    unavailable.insert(SourceKey::Secrets);
                }
            }
            load_inputs_from_store_mode(
                storage,
                store,
                signer,
                &sources,
                runtime,
                partial,
                unavailable,
            )
            .map_err(anyhow::Error::msg)
        })
        .map_err(|error| format!("load viewer storage: {error:#}"))
}

/// One scope's facts, read through its attached Rank9 cover with the
/// residual reported as pending: the stored facts, the revision
/// that identifies exactly what that read holds (the foundations it could
/// not read included), and those foundations. Raw payload arrival alone
/// does not advance it. Invalid selected attachment bytes still fail the read.
fn attached_facts_and_revision<R>(
    snapshot: &R,
    rank9: Collection<Rank9AcceleratedSuccinctArchiveBlob>,
) -> anyhow::Result<(FactArchive, DatasetRevision, Support)>
where
    R: triblespace::core::repo::StoreRead,
{
    let attached = snapshot
        .attached_acquiring(rank9)
        .map_err(|error| anyhow::anyhow!("attach the Rank9 fact cover: {error}"))?;
    let read =
        triblespace::core::collection::succinctarchive_union::read_attached_acquiring(&attached)?;
    let revision = DatasetRevision::from_attached(&attached, read.unread());
    let (facts, unread) = read.into_parts();
    Ok((facts, revision, unread))
}

/// A note for an index over the same foundations as its facts, only where it
/// misses one the facts did not.
fn index_note(label: &str, unread: &Support, facts_unread: &Support) -> Option<String> {
    unread
        .members()
        .any(|member| !facts_unread.contains(member))
        .then(|| crate::storage::unread_foundations_note(label, unread))
        .flatten()
}

fn load_inputs_from_store(
    storage: &Storage,
    pile: &mut FacultyStore,
    signer: &ed25519_dalek::SigningKey,
    sources: &BTreeSet<SourceKey>,
    runtime: &std::sync::Arc<tokio::runtime::Runtime>,
) -> Result<LoadedInputs, String> {
    load_inputs_from_store_mode(
        storage,
        pile,
        signer,
        sources,
        runtime,
        false,
        BTreeSet::new(),
    )
}

fn load_inputs_from_store_mode(
    storage: &Storage,
    pile: &mut FacultyStore,
    signer: &ed25519_dalek::SigningKey,
    requested: &BTreeSet<SourceKey>,
    runtime: &std::sync::Arc<tokio::runtime::Runtime>,
    partial: bool,
    unavailable: BTreeSet<SourceKey>,
) -> Result<LoadedInputs, String> {
    let mut prepared = prepare_inputs(pile, requested, partial, unavailable, |pile, scope| {
        storage.open_collection_local(pile, scope, signer.verifying_key())
    })?;

    let secrets = if prepared.sources.contains(&SourceKey::Secrets) {
        let observed = (|| {
            let collection = storage
                .open_secrets_collection(pile, signer.verifying_key(), runtime, false)
                .map_err(|error| format!("open Secrets: {error:#}"))?;
            let snapshot = runtime
                .block_on(secret_storage::ensure_and_snapshot(
                    pile, collection, signer,
                ))
                .map_err(|error| format!("observe Secrets: {error:#}"))?;
            Ok::<_, String>((collection, snapshot))
        })();
        match observed {
            Ok(value) => Some(value),
            Err(error) if !partial => return Err(error),
            Err(_) => {
                prepared.unavailable.insert(SourceKey::Secrets);
                None
            }
        }
    } else {
        None
    };

    // One final native observation serves every admitted dataset and index.
    // Failure of an optional role never causes another view to select again.
    let snapshot = match &secrets {
        Some((_, secrets)) => secrets.store_snapshot().clone(),
        None => pile
            .snapshot()
            .map_err(|error| format!("freeze viewer observation: {error}"))?,
    };
    let snapshot = WidgetReader::Acquiring(AcquiringReader::new(snapshot, runtime.clone()));
    read_inputs(
        snapshot,
        secrets.map(|(collection, _)| collection),
        prepared,
        partial,
    )
}

// Operation-local descriptor setup only; consumed before returning an observation.
// The BTree sets below are scratch for this single load, not a retained catalogue.
struct PreparedInputs {
    sources: BTreeSet<SourceKey>,
    unavailable: BTreeSet<SourceKey>,
    collections: Vec<(
        Id,
        &'static str,
        Collection<Rank9AcceleratedSuccinctArchiveBlob>,
    )>,
    goals_register: Option<Collection<LwwRegisterBlob>>,
    knowledge_latest: Option<Collection<LatestBlob>>,
}

fn prepare_inputs<S>(
    pile: &mut S,
    requested: &BTreeSet<SourceKey>,
    partial: bool,
    mut unavailable: BTreeSet<SourceKey>,
    mut open_source: impl FnMut(
        &mut S,
        Id,
    ) -> anyhow::Result<
        Collection<triblespace::prelude::blobencodings::SimpleArchive>,
    >,
) -> Result<PreparedInputs, String>
where
    S: CollectionStoreExt + SnapshotSource,
    S::Snapshot: BlobStoreGet + CapabilityProofRead,
{
    let mut sources = requested.clone();
    let mut collections = Vec::new();
    let mut goals_register = None;
    let mut knowledge_latest = None;
    for (scope, label) in collection_scopes(&sources) {
        let registration = (|| {
            let source =
                open_source(pile, scope).map_err(|error| format!("open {label}: {error:#}"))?;
            let succinct = pile
                .attach::<SuccinctArchiveBlob>(source, ())
                .map_err(|error| format!("attach {label}: {error:#}"))?;
            let rank9 = pile
                .attach::<Rank9AcceleratedSuccinctArchiveBlob>(source, succinct)
                .map_err(|error| format!("attach {label}: {error:#}"))?;
            if scope == GOALS_SCOPE_ID {
                goals_register = Some(
                    crate::goals::status_register_for_source(pile, source)
                        .map_err(|error| format!("attach Goals status: {error:#}"))?,
                );
            }
            if scope == KNOWLEDGE_SCOPE_ID {
                knowledge_latest = Some(
                    crate::knowledge::latest_for_source(pile, source)
                        .map_err(|error| format!("attach Knowledge supersession: {error:#}"))?,
                );
            }
            Ok::<_, String>(rank9)
        })();
        match registration {
            Ok(rank9) => collections.push((scope, label, rank9)),
            Err(error) if !partial => return Err(error),
            Err(_) => unavailable.extend(remove_scope(&mut sources, scope)),
        }
    }

    Ok(PreparedInputs {
        sources,
        unavailable,
        collections,
        goals_register,
        knowledge_latest,
    })
}

fn read_inputs(
    snapshot: WidgetReader,
    secrets: Option<secret_storage::SecretsCollection>,
    prepared: PreparedInputs,
    partial: bool,
) -> Result<LoadedInputs, String> {
    let PreparedInputs {
        mut sources,
        mut unavailable,
        collections,
        goals_register,
        knowledge_latest,
    } = prepared;
    let secrets = match secrets {
        Some(collection) => {
            match secret_storage::snapshot_acquiring(snapshot.clone(), collection) {
                Ok(snapshot) => Some(LoadedSecrets::new(snapshot)),
                Err(error) if !partial => return Err(format!("read Secrets: {error:#}")),
                Err(_) => {
                    unavailable.insert(SourceKey::Secrets);
                    None
                }
            }
        }
        None => None,
    };

    let mut datasets = BTreeMap::new();
    for (scope, label, rank9) in collections {
        if scope == SPAWN_SCOPE_ID {
            match snapshot.attached_acquiring(rank9) {
                Ok(selected) => {
                    datasets.insert(
                        SourceKey::Spawn,
                        LoadedDataset::deferred_spawn(snapshot.clone(), selected),
                    );
                }
                Err(error) if !partial => return Err(format!("select {label}: {error:#}")),
                Err(_) => {
                    unavailable.insert(SourceKey::Spawn);
                }
            }
            continue;
        }
        let read = (|| {
            let (facts, mut revision, unread) = attached_facts_and_revision(&snapshot, rank9)
                .map_err(|error| format!("read {label}: {error:#}"))?;
            let mut notes: Vec<String> = crate::storage::unread_foundations_note(label, &unread)
                .into_iter()
                .collect();
            let mut lww = BTreeMap::new();
            let mut latest = BTreeMap::new();
            if scope == GOALS_SCOPE_ID {
                if let Some(target) = goals_register {
                    let collection = snapshot
                        .attached_acquiring(target)
                        .map_err(|e| e.to_string())?;
                    let (index, status_unread) = collection
                        .read_acquiring::<LwwIndex>()
                        .map_err(|e| e.to_string())?
                        .into_parts();
                    notes.extend(index_note("Goals status", &status_unread, &unread));
                    revision.include_attached(&collection, &status_unread);
                    lww.insert(
                        (
                            crate::schemas::compass::board::status_of.id(),
                            triblespace::core::metadata::created_at.id(),
                        ),
                        index.query().map_err(|e| e.to_string())?,
                    );
                }
            }
            if scope == KNOWLEDGE_SCOPE_ID {
                if let Some(target) = knowledge_latest {
                    let collection = snapshot
                        .attached_acquiring(target)
                        .map_err(|e| e.to_string())?;
                    let (index, latest_unread) = collection
                        .read_acquiring::<LatestIndex>()
                        .map_err(|e| e.to_string())?
                        .into_parts();
                    notes.extend(index_note(
                        "Knowledge supersession",
                        &latest_unread,
                        &unread,
                    ));
                    revision.include_attached(&collection, &latest_unread);
                    latest.insert(triblespace::core::metadata::supersedes.id(), index);
                }
            }
            Ok::<_, String>((facts, revision, Arc::new(notes), lww, latest))
        })();
        match read {
            Ok((facts, revision, notes, lww, latest)) => {
                for source in COLLECTION_SOURCE_CATALOG
                    .iter()
                    .filter(|entry| entry.scope == scope && sources.contains(&entry.key))
                {
                    datasets.insert(
                        source.key,
                        LoadedDataset::new(
                            facts.clone(),
                            revision,
                            Arc::clone(&notes),
                            snapshot.clone(),
                            lww.clone(),
                            latest.clone(),
                        ),
                    );
                }
            }
            Err(error) if !partial => return Err(error),
            Err(_) => unavailable.extend(remove_scope(&mut sources, scope)),
        }
    }
    Ok(LoadedInputs {
        datasets,
        secrets,
        unavailable,
    })
}

fn render_banner(ctx: &mut CardCtx<'_>, icon: &str, message: &str, color: egui::Color32) {
    let ui = ctx.ui_mut();
    let background = egui::Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 40);
    egui::Frame::NONE
        .fill(background)
        .stroke(egui::Stroke::new(1.0, color))
        .corner_radius(egui::CornerRadius::same(3))
        .inner_margin(egui::Margin::symmetric(8, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.label(egui::RichText::new(icon).small().color(color));
                ui.label(
                    egui::RichText::new(message)
                        .monospace()
                        .small()
                        .color(color),
                );
            });
        });
}

const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<StorageState>();
};

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeSet;
    use std::fs::File;

    use anybytes::View;
    use ed25519_dalek::SigningKey;
    use hifitime::Epoch;
    use triblespace::core::metadata;
    use triblespace::core::repo::{BlobStoreGet, StoreSnapshot};
    use triblespace::macros::{entity, find, pattern};
    use triblespace::prelude::*;

    use crate::storage::{load_signer, open_pile_strict, open_pile_strict_as};
    use crate::test_support::initialize_open_collection_fixture;

    #[test]
    fn resident_workspace_tracks_late_payload_reads_and_invalidates_payload_caches() {
        use triblespace::core::collection::{AdmissionPolicy, CollectionPolicy};
        use triblespace::core::repo::{BlobStorePut, StoreChanges};
        let file = tempfile::NamedTempFile::new().unwrap();
        let signer = SigningKey::from_bytes(&[17; 32]);
        let mut pile = Pile::open_as(file.path(), signer.verifying_key()).unwrap();
        let teams = pile
            .collection(
                "live widget fixture",
                CollectionPolicy::new(AdmissionPolicy::Open, AdmissionPolicy::Open),
            )
            .unwrap();
        let routes = CollectionRoutes::from([(TEAMS_SCOPE_ID, teams.handle())]);
        let tracker = DependencyTracker::default();
        // Settle derived covers first; subsequent payload writes have no facts.
        drop(
            pollster::block_on(WorkspaceObservation::observe(&mut pile, &signer, &routes)).unwrap(),
        );
        let baseline = pile.snapshot().unwrap();
        let first = pollster::block_on(WorkspaceObservation::observe_tracked(
            &mut pile,
            &signer,
            &routes,
            tracker.clone(),
        ))
        .unwrap()
        .with_presentation_revision(1);
        let context = first.loaded_context();
        let dataset = context.dataset(SourceKey::Teams).unwrap();
        let clone = dataset.reader.clone();
        let body: Blob<blobencodings::UTF8String> = "late name or body".to_blob();
        let handle = body.get_handle();
        pile.put::<blobencodings::UTF8String, _>("unrelated payload")
            .unwrap();
        let unrelated = pile.snapshot().unwrap();
        assert_eq!(
            unrelated.changes_for(&baseline, &tracker.lock().unwrap()),
            StoreChanges::NONE
        );
        pile.put::<blobencodings::UTF8String, _>(body).unwrap();
        let arrived = pile.snapshot().unwrap();
        assert_eq!(
            arrived.changes_for(&baseline, &tracker.lock().unwrap()),
            StoreChanges::NONE
        );
        // UI reads occur after observe returned, through cloned resident readers.
        assert!(clone
            .get::<Blob<blobencodings::UTF8String>, _>(handle)
            .is_err());
        assert_ne!(
            arrived.changes_for(&baseline, &tracker.lock().unwrap()),
            StoreChanges::NONE
        );
        let second = pollster::block_on(WorkspaceObservation::observe(&mut pile, &signer, &routes))
            .unwrap()
            .with_presentation_revision(2);
        let context = second.loaded_context();
        let fresh = context.dataset(SourceKey::Teams).unwrap();
        assert!(fresh
            .reader
            .get::<Blob<blobencodings::UTF8String>, _>(handle)
            .is_ok());
        assert_ne!(
            dataset.revision, fresh.revision,
            "payload-only arrivals invalidate presentation caches"
        );
        assert_eq!(
            find!(name: Inline<_>, pattern!(dataset.facts, [{ metadata::name: ?name }])).count(),
            find!(name: Inline<_>, pattern!(fresh.facts, [{ metadata::name: ?name }])).count(),
            "no relation edit was needed",
        );
        let tracker = DependencyTracker::default();
        let baseline = pile.snapshot().unwrap();
        let _stable = pollster::block_on(WorkspaceObservation::observe_tracked(
            &mut pile,
            &signer,
            &routes,
            tracker.clone(),
        ))
        .unwrap();
        pile.commit(
            teams,
            &signer,
            entity! { metadata::name: "external relation change" },
        )
        .unwrap();
        assert_ne!(
            pile.snapshot()
                .unwrap()
                .changes_for(&baseline, &tracker.lock().unwrap()),
            StoreChanges::NONE
        );
        pile.close().unwrap();
    }

    #[test]
    fn resident_workspace_observes_one_live_owner_and_keeps_frozen_views() {
        use triblespace::core::collection::{AdmissionPolicy, CollectionPolicy};
        let file = tempfile::NamedTempFile::new().unwrap();
        let signer = SigningKey::from_bytes(&[17; 32]);
        let mut pile = Pile::open_as(file.path(), signer.verifying_key()).unwrap();
        let open = CollectionPolicy::new(AdmissionPolicy::Open, AdmissionPolicy::Open);
        let teams = pile.collection("team fixture", open.clone()).unwrap();
        let messages = pile.collection("message fixture", open).unwrap();
        let denied = pile
            .collection(
                "files fixture",
                CollectionPolicy::new(
                    AdmissionPolicy::direct(SigningKey::from_bytes(&[18; 32]).verifying_key()),
                    AdmissionPolicy::Open,
                ),
            )
            .unwrap();
        let routes = CollectionRoutes::from([
            (TEAMS_SCOPE_ID, teams.handle()),
            (MESSAGES_SCOPE_ID, messages.handle()),
            (FILES_SCOPE_ID, denied.handle()),
        ]);
        pile.commit(teams, &signer, entity! { metadata::name: "before" })
            .unwrap();
        // This fixture's initial positive view is published by the owner;
        // observing the workspace itself must not construct its raw commit.
        drop(
            pollster::block_on(crate::storage::ensure_downstream(&mut pile, teams, &signer))
                .unwrap(),
        );
        let first =
            pollster::block_on(WorkspaceObservation::observe(&mut pile, &signer, &routes)).unwrap();
        let first_context = first.loaded_context();
        let before = first_context.dataset(SourceKey::Teams).unwrap();
        let peer = first_context.dataset(SourceKey::Messages).unwrap();
        assert!(matches!(before.reader, WidgetReader::Resident(_)));
        assert!(before.reader.changes_since(peer.reader).is_empty());
        assert_eq!(
            first.unavailable_labels().collect::<Vec<_>>(),
            vec!["Files"]
        );
        assert!(!first_context.contains(SourceKey::Knowledge));
        pile.commit(teams, &signer, entity! { metadata::name: "after" })
            .unwrap();
        let second =
            pollster::block_on(WorkspaceObservation::observe(&mut pile, &signer, &routes)).unwrap();
        assert_ne!(
            before.revision,
            second
                .loaded_context()
                .dataset(SourceKey::Teams)
                .unwrap()
                .revision
        );
        assert_eq!(
            find!(name: Inline<_>, pattern!(before.facts, [{ metadata::name: ?name }])).count(),
            1
        );
        drop(first);
        drop(second);
        // Closing every viewer observation leaves the same mutable owner usable.
        pile.commit(teams, &signer, entity! { metadata::name: "after close" })
            .unwrap();
        pile.close().unwrap();
    }

    #[test]
    fn resident_workspace_preserves_delivered_secrets_without_read_authority() {
        use triblespace::core::collection::{AdmissionPolicy, CollectionPolicy};
        let file = tempfile::NamedTempFile::new().unwrap();
        let signer = SigningKey::from_bytes(&[19; 32]);
        let mut pile = Pile::open_as(file.path(), signer.verifying_key()).unwrap();
        let source = pile
            .collection(
                "secret fixture",
                CollectionPolicy::new(
                    AdmissionPolicy::direct(SigningKey::from_bytes(&[20; 32]).verifying_key()),
                    AdmissionPolicy::direct(signer.verifying_key()),
                ),
            )
            .unwrap();
        let at = Epoch::from_unix_seconds(100.0);
        let sealed = crate::secrets::seal_version(
            "fixture",
            b"fixture only",
            [signer.verifying_key()],
            (at, at).try_to_inline().unwrap(),
        )
        .unwrap();
        let secret = sealed.secret;
        pile.commit(source, &signer, sealed.fragment).unwrap();
        assert!(crate::collection_names::open_workspace_read_in(
            &pile.snapshot().unwrap(),
            signer.verifying_key(),
            source.handle(),
        )
        .is_err());
        let routes = CollectionRoutes::from([(crate::secrets::DEFAULT_SCOPE_ID, source.handle())]);
        let observed =
            pollster::block_on(WorkspaceObservation::observe(&mut pile, &signer, &routes)).unwrap();
        assert_eq!(
            observed
                .loaded_context()
                .secrets()
                .unwrap()
                .snapshot
                .open(secret, &signer)
                .unwrap(),
            b"fixture only"
        );
        drop(observed);
        pile.close().unwrap();
    }

    fn create_pile(path: &Path) {
        File::create(path).unwrap();
        initialize_open_collection_fixture(path, None);
    }

    fn publish_reason(path: &Path, text: &str, second: f64) {
        let instant = Epoch::from_tai_seconds(second);
        crate::cognition::publish_event(
            path,
            None,
            crate::cognition::reason_fragment(
                None,
                None,
                text,
                None,
                (instant, instant).try_to_inline().unwrap(),
            ),
        )
        .unwrap();
    }

    fn publish_malformed_status(path: &Path) {
        let instant = Epoch::from_tai_seconds(3.0);
        use crate::schemas::status::{status, KIND_STATUS_UPDATE};
        let at: Inline<inlineencodings::NsTAIInterval> =
            (instant, instant).try_to_inline().unwrap();
        let mut fragment = entity! {
            metadata::tag: &KIND_STATUS_UPDATE,
            status::window: Id::new([0x31; 16]).unwrap(),
            status::text: "malformed",
            metadata::created_at: at,
        };
        let event = fragment.root().unwrap();
        fragment += entity! {
            ExclusiveId::force_ref(&event) @ metadata::tag: &Id::new([0x32; 16]).unwrap()
        };

        let signer = load_signer(path, None).unwrap();
        let mut pile = open_pile_strict(path).unwrap();
        let collection = crate::collection_names::open(
            &mut pile,
            crate::schemas::status::DEFAULT_SCOPE_ID,
            signer.verifying_key(),
        )
        .unwrap();
        pile.commit(collection, &signer, fragment).unwrap();
        pile.close().unwrap();
    }

    fn visible_keys(context: WidgetContext<'_>) -> BTreeSet<SourceKey> {
        SourceKey::ALL
            .into_iter()
            .filter(|key| context.contains(*key))
            .collect()
    }

    fn workspace_fixture() -> (tempfile::TempDir, Storage) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("views.pile");
        let key = directory.path().join("views.key");
        File::create(&path).unwrap();
        let signer = crate::storage::initialize_signer(&path, Some(&key)).unwrap();
        let storage =
            Storage::shared_local(path, Some(key)).with_expected_signer(signer.verifying_key());
        (directory, storage)
    }

    #[test]
    fn workspace_views_require_explicit_routes_without_opening_a_default_pile() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("absent.pile");
        assert!(StorageState::for_workspace(Storage::shared_local(path.clone(), None)).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn workspace_views_omit_missing_roles_and_keep_readable_data_when_another_role_is_denied() {
        use triblespace::core::collection::{AdmissionPolicy, CollectionPolicy};
        let (_directory, storage) = workspace_fixture();
        let other = SigningKey::from_bytes(&[31; 32]).verifying_key();
        let routes = storage
            .with_store(|store, signer, _| {
                let teams = store.collection(
                    "teams",
                    CollectionPolicy::new(
                        AdmissionPolicy::Open,
                        AdmissionPolicy::direct(signer.verifying_key()),
                    ),
                )?;
                let files = store.collection(
                    "files",
                    CollectionPolicy::new(
                        AdmissionPolicy::direct(other),
                        AdmissionPolicy::direct(signer.verifying_key()),
                    ),
                )?;
                // A readable same-name alternative exists. It must never be used
                // when the workspace's exact Files root refuses READ.
                let _other_files = store.collection(
                    "files",
                    CollectionPolicy::new(AdmissionPolicy::Open, AdmissionPolicy::Open),
                )?;
                Ok(BTreeMap::from([
                    (TEAMS_SCOPE_ID, teams.handle()),
                    (FILES_SCOPE_ID, files.handle()),
                ]))
            })
            .unwrap();
        let mut views =
            StorageState::for_workspace(storage.clone().with_collections(routes)).unwrap();
        views.refresh();
        assert!(views.error().is_none(), "{:?}", views.error());
        assert_eq!(
            visible_keys(views.loaded_context()),
            BTreeSet::from([SourceKey::Teams])
        );
        assert_eq!(
            views.unavailable_labels().collect::<Vec<_>>(),
            vec!["Files"]
        );
        assert!(!views.loaded_context().contains(SourceKey::Knowledge));
        storage.close().unwrap();
    }

    #[test]
    fn workspace_secrets_keep_delivered_wrap_possession_without_collection_read() {
        use triblespace::core::collection::{AdmissionPolicy, CollectionPolicy};
        let (_directory, storage) = workspace_fixture();
        let replication_owner = SigningKey::from_bytes(&[29; 32]).verifying_key();
        let (handle, secret, signer) = storage
            .with_store(|store, signer, _| {
                let source = store.collection(
                    "secrets",
                    CollectionPolicy::new(
                        AdmissionPolicy::direct(replication_owner),
                        AdmissionPolicy::direct(signer.verifying_key()),
                    ),
                )?;
                let time = Epoch::from_unix_seconds(100.0);
                let sealed = crate::secrets::seal_version(
                    "delivered",
                    b"local envelope",
                    [signer.verifying_key()],
                    (time, time)
                        .try_to_inline()
                        .expect("equal fixture endpoints form a valid interval"),
                )?;
                let secret = sealed.secret;
                store.commit(source, signer, sealed.fragment)?;
                Ok((source.handle(), secret, signer.clone()))
            })
            .unwrap();
        let selected = storage
            .clone()
            .with_collections(BTreeMap::from([(crate::secrets::DEFAULT_SCOPE_ID, handle)]));
        assert!(
            selected
                .with_store(|store, signer, runtime| {
                    selected.open_collection_read(
                        store,
                        crate::secrets::DEFAULT_SCOPE_ID,
                        signer.verifying_key(),
                        runtime,
                    )
                })
                .is_err(),
            "the fixture must really deny collection READ"
        );
        let mut views = StorageState::for_workspace(selected).unwrap();
        views.refresh();
        assert!(views.error().is_none(), "{:?}", views.error());
        let secret_view = views.loaded_context().secrets().unwrap();
        assert_eq!(
            secret_view.snapshot.open(secret, &signer).unwrap(),
            b"local envelope"
        );
        assert!(views.unavailable_labels().next().is_none());
        storage.close().unwrap();
    }

    #[test]
    fn workspace_views_share_one_observation_and_do_not_require_optional_relations() {
        use triblespace::core::collection::{AdmissionPolicy, CollectionPolicy};
        let (_directory, storage) = workspace_fixture();
        let routes = storage
            .with_store(|store, _, _| {
                let policy = CollectionPolicy::new(AdmissionPolicy::Open, AdmissionPolicy::Open);
                let messages = store.collection("message", policy.clone())?;
                let teams = store.collection("teams", policy)?;
                Ok(BTreeMap::from([
                    (MESSAGES_SCOPE_ID, messages.handle()),
                    (TEAMS_SCOPE_ID, teams.handle()),
                ]))
            })
            .unwrap();
        let mut views =
            StorageState::for_workspace(storage.clone().with_collections(routes)).unwrap();
        views.refresh();
        assert!(views.error().is_none(), "{:?}", views.error());
        let context = views.loaded_context();
        let messages = context.dataset(SourceKey::Messages).unwrap();
        let teams = context.dataset(SourceKey::Teams).unwrap();
        assert!(context.dataset(SourceKey::Relations).is_none());
        assert!(messages.reader.changes_since(teams.reader).is_empty());
        assert!(teams.reader.changes_since(messages.reader).is_empty());
        storage.close().unwrap();
    }

    #[test]
    fn explicit_key_is_lazy_used_for_loading_and_invalidates_previous_identity() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("explicit.pile");
        create_pile(&path);
        let key = directory.path().join("launcher.key");
        std::fs::rename(crate::storage::signer_path(&path, None), &key).unwrap();
        let before = std::fs::metadata(&path).unwrap().len();
        let mut storage =
            StorageState::for_sources(&path, [SourceKey::Teams]).with_key_path(Some(key));
        assert!(storage.datasets.is_none());
        assert_eq!(std::fs::metadata(&path).unwrap().len(), before);
        assert!(storage.context().contains(SourceKey::Teams));
        assert!(storage.error().is_none());

        // Rebinding an already-loaded value cannot keep the previous key's
        // readable datasets while waiting for a reload under another identity.
        storage = storage.with_key_path(Some(directory.path().join("missing.key")));
        assert!(storage.datasets.is_none());
        assert!(!storage.context().contains(SourceKey::Teams));
        assert!(storage.error().unwrap().contains("missing.key"));
    }

    #[test]
    fn storage_load_settles_once_and_context_refresh_is_idempotent() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("viewer.pile");
        create_pile(&path);
        publish_reason(&path, "read only", 1.0);
        let length = std::fs::metadata(&path).unwrap().len();
        let mut storage = StorageState::new(&path);

        let context = storage.context();
        let view = context.dataset(SourceKey::Reason).unwrap();
        let text = find!(
            text: Inline<inlineencodings::Handle<blobencodings::UTF8String>>,
            pattern!(view.facts, [{ metadata::tag: crate::schemas::reason::KIND_REASON_ID,
                                    crate::schemas::reason::reason_schema::text: ?text }])
        )
        .next()
        .unwrap();
        let text: View<str> = view.reader.get(text).unwrap();
        assert_eq!(&*text, "read only");
        assert_eq!(view.facts.segment_count(), 1);
        let settled_length = std::fs::metadata(&path).unwrap().len();
        assert!(settled_length >= length);
        let _ = storage.context();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), settled_length);
        assert!(storage.error().is_none());
    }

    #[test]
    fn open_reloads_the_same_path_and_stamp_refreshes_live_data() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("reload.pile");
        create_pile(&path);
        publish_reason(&path, "first", 1.0);
        let mut storage = StorageState::new(&path);
        let first = storage
            .context()
            .dataset(SourceKey::Reason)
            .unwrap()
            .revision;

        publish_reason(&path, "second", 2.0);
        storage.set_pile_path(&path);
        let second = storage
            .context()
            .dataset(SourceKey::Reason)
            .unwrap()
            .revision;
        assert_ne!(second, first);

        publish_reason(&path, "third", 3.0);
        let third = storage
            .context()
            .dataset(SourceKey::Reason)
            .unwrap()
            .revision;
        assert_ne!(third, second);
    }

    #[test]
    fn shared_collection_is_loaded_once_and_exposed_under_semantic_keys() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("shared.pile");
        create_pile(&path);
        publish_reason(&path, "one source", 1.0);
        let mut storage = StorageState::new(&path);
        let context = storage.context();

        let reason = context.dataset(SourceKey::Reason).unwrap();
        let diagnostics = context.dataset(SourceKey::Diagnostics).unwrap();
        assert_eq!(reason.revision, diagnostics.revision);
        assert!(reason.facts.iter().eq(diagnostics.facts.iter()));
        let secrets = context.secrets().unwrap();
        let secrets_reader = secrets.snapshot.store_snapshot();
        for reader in [reason.reader, diagnostics.reader] {
            assert!(reader.changes_since(secrets_reader).is_empty());
            assert!(secrets_reader.changes_since(reader).is_empty());
        }
    }

    #[test]
    fn scoped_storage_expands_dependencies_without_exposing_shared_scope_aliases() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("triage.pile");
        create_pile(&path);
        let mut storage = StorageState::for_sources(&path, [SourceKey::Diagnostics]);

        assert_eq!(
            visible_keys(storage.context()),
            BTreeSet::from([
                SourceKey::Profiles,
                SourceKey::Messages,
                SourceKey::Relations,
                SourceKey::Secrets,
                SourceKey::Diagnostics,
            ])
        );
        assert!(storage.error().is_none());
    }

    #[test]
    fn source_dependencies_expand_transitively() {
        assert_eq!(
            source_closure([SourceKey::Mail]),
            BTreeSet::from([SourceKey::Mail, SourceKey::Relations])
        );
        assert_eq!(
            source_closure([SourceKey::Profiles]),
            BTreeSet::from([SourceKey::Profiles, SourceKey::Secrets])
        );
        assert_eq!(
            source_closure([SourceKey::Teams]),
            BTreeSet::from([SourceKey::Teams])
        );
        for source in [SourceKey::Messages, SourceKey::Events] {
            assert_eq!(
                source_closure([source]),
                BTreeSet::from([source, SourceKey::Relations])
            );
        }
    }

    /// The revision names exactly what a fact read holds. A commit that
    /// arrives without an attachment remains pending. Raw byte arrival alone
    /// cannot expose it; a stored host attachment changes the visible facts
    /// and revision. This exercises the same loader as ordinary Data.
    #[test]
    fn dataset_revision_tracks_pending_foundations_then_stored_attachments() {
        use triblespace::core::blob::encodings::simplearchive::SimpleArchive;
        use triblespace::core::blob::{Blob, IntoBlob};
        use triblespace::core::collection::{
            empty_metadata_handle, CollectionCommit, CollectionRecord, CollectionStore,
        };
        use triblespace::core::inline::encodings::hash::Handle;
        use triblespace::core::repo::memoryrepo::MemoryRepo;
        use triblespace::core::repo::BlobStorePut;

        let signer = SigningKey::from_bytes(&[36; 32]);
        let mut store = MemoryRepo::for_host(signer.verifying_key());
        let source = store
            .collection(
                "residual-cache",
                crate::collection_names::private_policy(signer.verifying_key()),
            )
            .unwrap();
        let (_, rank9) = crate::storage::fact_pair(&mut store, source).unwrap();
        let (first, second, third) = (genid(), genid(), genid());
        store
            .commit(
                source,
                &signer,
                entity! { &first @ metadata::name: "first" },
            )
            .unwrap();
        crate::storage::carry_facts(&mut store, source, &signer);
        let (_, attached, _) =
            attached_facts_and_revision(&store.snapshot().unwrap(), rank9).unwrap();

        // A commit arrives, as sync would bring it; nothing attaches it.
        store
            .commit(
                source,
                &signer,
                entity! { &second @ metadata::name: "second" },
            )
            .unwrap();
        let (facts, read_raw, unread) =
            attached_facts_and_revision(&store.snapshot().unwrap(), rank9).unwrap();
        assert_eq!(unread.len(), 1);
        let names = |facts: &FactArchive| -> BTreeSet<Id> {
            find!(id: Id, pattern!(facts, [{ ?id @ metadata::name: _?name }])).collect()
        };
        assert_eq!(names(&facts), BTreeSet::from([first.id]));
        assert_ne!(attached, read_raw, "the cover alone did not change");

        // A commit whose payload is not here yet.
        let payload: Blob<SimpleArchive> = entity! { &third @ metadata::name: "third" }
            .into_facts()
            .to_blob();
        store
            .insert(CollectionRecord::Commit(CollectionCommit::sign(
                &signer,
                source.handle(),
                Handle::<SimpleArchive>::to_hash(payload.get_handle()),
                empty_metadata_handle(),
            )))
            .unwrap();
        let missing_snapshot = store.snapshot().unwrap();
        let (facts, gap, unread) = attached_facts_and_revision(&missing_snapshot, rank9)
            .expect("a foreground dataset shows what is here");
        assert_eq!(names(&facts), BTreeSet::from([first.id]));
        assert_eq!(unread.len(), 2);
        assert!(unread.contains(payload.get_handle()));
        // The revision law distinguishes this partial observation from the
        // earlier one and from the later complete one.
        assert_ne!(read_raw, gap);
        store.put::<SimpleArchive, _>(payload).unwrap();
        let (facts, arrived, unread) =
            attached_facts_and_revision(&store.snapshot().unwrap(), rank9).unwrap();
        assert_eq!(unread.len(), 2);
        assert_eq!(names(&facts), BTreeSet::from([first.id]));
        assert_eq!(
            gap, arrived,
            "raw byte arrival cannot advance a stored-only view"
        );
        let records_before = store.snapshot().unwrap().records().unwrap().count();
        let _ = attached_facts_and_revision(&store.snapshot().unwrap(), rank9).unwrap();
        assert_eq!(
            store.snapshot().unwrap().records().unwrap().count(),
            records_before
        );
        crate::storage::carry_facts(&mut store, source, &signer);
        let (facts, published, unread) =
            attached_facts_and_revision(&store.snapshot().unwrap(), rank9).unwrap();
        assert!(unread.is_empty());
        assert_eq!(
            names(&facts),
            BTreeSet::from([first.id, second.id, third.id])
        );
        assert_ne!(gap, published, "persisted attachment advances the revision");
    }

    /// Exercise ordinary Data's routed backend, not only the fact-read helper.
    /// The same frozen pending view must remain pending after later upkeep.
    #[test]
    fn workspace_data_reports_raw_only_gap_then_reads_stored_host_map() {
        use triblespace::core::collection::{AdmissionPolicy, CollectionPolicy};
        let file = tempfile::NamedTempFile::new().unwrap();
        let signer = SigningKey::from_bytes(&[36; 32]);
        let mut pile = Pile::open_as(file.path(), signer.verifying_key()).unwrap();
        let source = pile
            .collection(
                "Data stored-only fixture",
                CollectionPolicy::new(AdmissionPolicy::Open, AdmissionPolicy::Open),
            )
            .unwrap();
        let routes = CollectionRoutes::from([(TEAMS_SCOPE_ID, source.handle())]);
        pile.commit(source, &signer, entity! { metadata::name: "pending team" })
            .unwrap();
        let before = pile.snapshot().unwrap().records().unwrap().count();
        let pending =
            pollster::block_on(WorkspaceObservation::observe(&mut pile, &signer, &routes)).unwrap();
        let pending_context = pending.loaded_context();
        let gap = pending_context.dataset(SourceKey::Teams).unwrap();
        assert!(gap.facts.iter().next().is_none());
        assert!(
            !gap.notes.is_empty(),
            "Data must not report empty as complete"
        );
        assert_eq!(
            pile.snapshot().unwrap().records().unwrap().count(),
            before,
            "ordinary Data observation must publish no MAP or merge"
        );
        drop(
            pollster::block_on(crate::storage::ensure_downstream(
                &mut pile, source, &signer,
            ))
            .unwrap(),
        );
        let complete =
            pollster::block_on(WorkspaceObservation::observe(&mut pile, &signer, &routes)).unwrap();
        let complete_context = complete.loaded_context();
        let visible = complete_context.dataset(SourceKey::Teams).unwrap();
        assert_eq!(
            find!(name: Inline<_>, pattern!(visible.facts, [{ metadata::name: ?name }])).count(),
            1
        );
        assert!(visible.notes.is_empty());
        assert_ne!(gap.revision, visible.revision);
        assert!(
            gap.facts.iter().next().is_none(),
            "a retained Data view must not reselect"
        );
        assert!(!gap.notes.is_empty());
        drop((pending, complete));
        pile.close().unwrap();
    }

    #[test]
    fn dataset_revision_changes_when_only_latest_cover_advances() {
        pollster::block_on(async {
            use triblespace::core::collection::latest::LatestBlob;

            let signer = SigningKey::from_bytes(&[35; 32]);
            let mut store = MemoryRepo::for_host(signer.verifying_key());
            let source = store
                .collection(
                    "latest-cache",
                    crate::collection_names::private_policy(signer.verifying_key()),
                )
                .unwrap();
            let target = store
                .attach::<LatestBlob>(source, metadata::supersedes.id())
                .unwrap();
            let root = genid();
            let next = genid();
            store
                .commit(source, &signer, entity! { &root @ metadata::name: "root" })
                .unwrap();
            drop(store.maintain_attached(target, &signer).await.unwrap());
            store
                .commit(
                    source,
                    &signer,
                    entity! { &next @ metadata::supersedes: &root },
                )
                .unwrap();
            let snapshot = store.snapshot().unwrap();
            let facts = snapshot.collection(source).unwrap();
            let lagging = snapshot.attached(target).unwrap();
            let (lagging_index, unread) = lagging.read::<LatestIndex>().unwrap().into_parts();
            let before = DatasetRevision::from_attached(&lagging, &unread);

            let ready = store.maintain_attached(target, &signer).await.unwrap();
            let advanced = ready.attached(target).unwrap();
            let (advanced_index, unread) = advanced.read::<LatestIndex>().unwrap().into_parts();
            let after = DatasetRevision::from_attached(&advanced, &unread);
            // Only upkeep makes the successor's stored index visible.
            assert_ne!(
                lagging_index.states().collect::<BTreeSet<_>>(),
                advanced_index.states().collect::<BTreeSet<_>>()
            );
            assert_ne!(
                before, after,
                "index-only progress invalidates widget projections"
            );
            assert_eq!(facts.cover(), ready.collection(source).unwrap().cover());
        });
    }

    #[test]
    fn knowledge_dataset_attaches_positive_latest_index_without_advancing_source() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("wiki-latest.pile");
        create_pile(&path);
        let signer = load_signer(&path, None).unwrap();
        let (author_fragment, author) = crate::knowledge::author_record(&signer.verifying_key());
        let instant = Epoch::from_tai_seconds(1.0);
        let (root_fragment, root) =
            crate::knowledge::revision_record(crate::knowledge::RevisionDraft {
                title: "root".to_owned(),
                content: "root".to_owned(),
                tags: BTreeSet::new(),
                predecessors: BTreeSet::new(),
                author,
                authored_at: (instant, instant).try_to_inline().unwrap(),
            })
            .unwrap();
        let (successor_fragment, successor) =
            crate::knowledge::revision_record(crate::knowledge::RevisionDraft {
                title: "successor".to_owned(),
                content: "successor".to_owned(),
                tags: BTreeSet::new(),
                predecessors: BTreeSet::from([root]),
                author,
                authored_at: (instant, instant).try_to_inline().unwrap(),
            })
            .unwrap();
        let mut pile = open_pile_strict_as(&path, signer.verifying_key()).unwrap();
        crate::knowledge::commit_collection(
            &mut pile,
            &signer,
            author_fragment + root_fragment + successor_fragment,
        )
        .unwrap();
        crate::knowledge::carry_for_tests(&mut pile, &signer);
        let collection =
            crate::collection_names::open(&mut pile, KNOWLEDGE_SCOPE_ID, signer.verifying_key())
                .unwrap();
        let store_snapshot = pile.snapshot().unwrap();
        let cover_before = collection.admitted(&store_snapshot).unwrap();
        pile.close().unwrap();

        let mut storage = StorageState::for_sources(&path, [SourceKey::Knowledge]);
        let dataset = storage.context().dataset(SourceKey::Knowledge).unwrap();
        let latest = dataset
            .latest_index(metadata::supersedes.id())
            .expect("Knowledge dataset carries its positive latest relation");
        let entries = crate::knowledge::entries(dataset.facts, latest);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].frontier.len(), 1);
        assert_eq!(entries[0].frontier[0].id, successor);

        let mut pile = open_pile_strict(&path).unwrap();
        let collection =
            crate::collection_names::open(&mut pile, KNOWLEDGE_SCOPE_ID, signer.verifying_key())
                .unwrap();
        let store_snapshot = pile.snapshot().unwrap();
        let cover_after = collection.admitted(&store_snapshot).unwrap();
        pile.close().unwrap();
        assert_eq!(cover_after, cover_before);
    }

    #[test]
    fn shared_scope_is_planned_once_but_each_requested_semantic_key_is_exposed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("shared-scoped.pile");
        create_pile(&path);
        publish_reason(&path, "shared", 1.0);

        let sources = source_closure([SourceKey::Reason, SourceKey::Diagnostics]);
        assert_eq!(
            collection_scopes(&sources)
                .into_iter()
                .filter(|(scope, _)| *scope == COGNITION_SCOPE_ID)
                .count(),
            1
        );

        let mut storage =
            StorageState::for_sources(&path, [SourceKey::Reason, SourceKey::Diagnostics]);
        let context = storage.context();
        let reason = context.dataset(SourceKey::Reason).unwrap();
        let diagnostics = context.dataset(SourceKey::Diagnostics).unwrap();
        assert_eq!(reason.revision, diagnostics.revision);
        assert!(reason.facts.iter().eq(diagnostics.facts.iter()));
    }

    #[test]
    fn malformed_source_facts_do_not_block_ordinary_source_attachment() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("malformed-unrelated.pile");
        create_pile(&path);
        publish_reason(&path, "still visible", 1.0);
        publish_malformed_status(&path);

        let mut narrow = StorageState::for_sources(&path, [SourceKey::Reason]);
        let context = narrow.context();
        assert!(context.dataset(SourceKey::Reason).is_some());
        assert!(context.dataset(SourceKey::Diagnostics).is_none());
        assert!(narrow.error().is_none());

        let mut full = StorageState::new(&path);
        let context = full.context();
        assert!(context.dataset(SourceKey::Reason).is_some());
        assert!(full.error().is_none());
    }

    #[test]
    fn full_storage_exposes_every_source() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("full.pile");
        create_pile(&path);
        let mut storage = StorageState::new(&path);

        let visible = visible_keys(storage.context());
        assert!(storage.error().is_none(), "{:?}", storage.error());
        assert_eq!(visible, SourceKey::ALL.into_iter().collect());
    }

    #[test]
    fn fixed_catalog_covers_every_fixed_source_key_once() {
        let catalog: BTreeSet<_> = COLLECTION_SOURCE_CATALOG
            .iter()
            .map(|source| source.key)
            .collect();
        let mut expected: BTreeSet<_> = SourceKey::ALL.into_iter().collect();
        expected.remove(&SourceKey::Secrets);
        assert_eq!(catalog, expected);
        assert_eq!(catalog.len(), COLLECTION_SOURCE_CATALOG.len());
        assert_eq!(
            COLLECTION_SOURCE_CATALOG
                .iter()
                .filter(|source| source.scope == COGNITION_SCOPE_ID)
                .count(),
            2
        );
    }

    #[test]
    fn secrets_is_a_dynamic_aggregate_not_a_fixed_dataset() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("secrets.pile");
        create_pile(&path);
        let mut storage = StorageState::for_sources(&path, [SourceKey::Secrets]);

        let context = storage.context();
        assert!(context.contains(SourceKey::Secrets));
        assert!(context.secrets().is_some());
        assert!(context.dataset(SourceKey::Secrets).is_none());
        assert!(storage.error().is_none());
    }

    #[test]
    fn missing_durable_signer_fails_without_mutating_the_pile() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("unsigned.pile");
        File::create(&path).unwrap();
        let length = std::fs::metadata(&path).unwrap().len();
        let mut storage = StorageState::new(&path);

        assert!(storage.context().dataset(SourceKey::Knowledge).is_none());
        let error = storage.error().expect("missing signer is surfaced");
        assert!(error.contains("load durable signing key"), "{error}");
        assert!(!crate::storage::signer_path(&path, None).exists());
        assert_eq!(std::fs::metadata(&path).unwrap().len(), length);
    }
}
