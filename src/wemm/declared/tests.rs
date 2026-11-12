//! Scratch piles only, no device: a stand-in model that derives an index
//! only while the model its descriptor names is the one bound, as the
//! runtime does (`WemmRuntime::matches`).
use super::*;
use crate::storage::{initialize_signer, underived};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use triblespace::core::blob::Blob;
use triblespace::core::collection::{
    CollectionDerive, CollectionOperationError, CollectionPolicy, CollectionRecord,
    CollectionStore, CollectionStoreExt, DeriveMapping, SourceLocator,
};
use triblespace::core::metadata;
use triblespace::core::trible::Fragment;
use triblespace_search::nvfp4::NvFp4CosineSet;
use triblespace_search::schemas::Embedding;
use triblespace_search::semantic_wemm::{
    WemmIndex, DEFAULT_WINDOW_CAP, GB10_NATIVE_BF16_PROFILE, GB10_NATIVE_W4A4_PROFILE,
    GB10_NATIVE_W4A4_VW4A16_PROFILE,
};

thread_local! {
    /// The model the stand-in runtime has bound on this thread, if any.
    static BOUND: Cell<Option<WemmIdentity>> = const { Cell::new(None) };
}

/// The WeMM mapping with the device replaced: computable only under the
/// model its own descriptor names, every foundation mapped to no rows.
struct StandIn(WemmIndex<Embedding>);

impl DeriveMapping for StandIn {
    type Source = SimpleArchive;
    type Target = NvFp4CosineSet<Embedding>;

    fn fragment(&self) -> Fragment {
        self.0.fragment()
    }

    fn bind(source: &Fragment, target: &Fragment) -> Result<Self, CollectionOperationError> {
        WemmIndex::<Embedding>::bind(source, target).map(Self)
    }

    fn computable_here(&self) -> bool {
        BOUND.get() == Some(self.0.identity())
    }

    fn map<R: StoreRead>(
        &self,
        _source: &Blob<SimpleArchive>,
        _reader: &R,
    ) -> Result<Blob<Self::Target>, CollectionOperationError> {
        assert!(self.computable_here(), "mapped under another model");
        NvFp4CosineSet::from_quantized_rows_4096([])
            .map_err(|e| CollectionOperationError::Fatal(e.to_string()))
    }
}

#[derive(Default)]
struct Log {
    binds: Vec<WemmIdentity>,
    maintained: Vec<(WemmIdentity, CollectionHandle)>,
}

/// Binds every model but the roots in `unbindable`, and maintains through
/// the stand-in with the model it was handed.
struct TestBinder {
    log: Rc<RefCell<Log>>,
    unbindable: Vec<Id>,
    keeps_every_bind: bool,
    /// How long a bind takes before it answers.
    bind_takes: Duration,
    /// How long maintaining one index takes.
    maintain_takes: Duration,
}

impl Binder for TestBinder {
    type Model = WemmIdentity;

    fn bind(&mut self, identity: WemmIdentity) -> Result<WemmIdentity> {
        self.log.borrow_mut().binds.push(identity);
        std::thread::sleep(self.bind_takes);
        anyhow::ensure!(
            !self.unbindable.contains(&identity.model_root),
            "WEMM_ROOT {:X} is not stated by the model collection",
            identity.model_root
        );
        Ok(identity)
    }

    fn keeps_every_bind(&self) -> bool {
        self.keeps_every_bind
    }

    fn maintain(
        &mut self,
        model: &WemmIdentity,
        store: &mut FacultyStore,
        source: Collection<SimpleArchive>,
        target: Target,
        signer: &SigningKey,
        runtime: &Arc<tokio::runtime::Runtime>,
    ) -> Result<Derived> {
        let before = underived(
            &AcquiringReader::new(store.snapshot()?, runtime.clone()),
            source,
            target,
        )?;
        std::thread::sleep(self.maintain_takes);
        BOUND.set(Some(*model));
        let maintained = runtime.block_on(store.maintain_with::<StandIn>(target, signer));
        BOUND.set(None);
        maintained.context("maintain through the stand-in")?;
        let missing = underived(
            &AcquiringReader::new(store.snapshot()?, runtime.clone()),
            source,
            target,
        )?;
        self.log
            .borrow_mut()
            .maintained
            .push((*model, target.handle()));
        Ok(Derived {
            index: target.handle(),
            derived: before - missing,
            missing,
            rows: 0,
            elapsed: Duration::ZERO,
        })
    }
}

const MEMBERS: [&str; 6] = ["one", "two", "three", "four", "five", "six"];

struct Fixture {
    _dir: tempfile::TempDir,
    storage: Storage,
    source: Collection<SimpleArchive>,
    policy: CollectionPolicy,
    members: Cell<usize>,
}

impl Fixture {
    /// A scratch pile with a source of `members` commits.
    fn new(members: usize) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let pile = dir.path().join("declared.pile");
        let key = dir.path().join("declared.key");
        std::fs::File::create(&pile).unwrap();
        initialize_signer(&pile, Some(&key)).unwrap();
        let storage = Storage::shared_local(pile, Some(key));
        let (source, policy) = storage
            .with_store(|store, signer, _| {
                let policy = crate::collection_names::private_policy(signer.verifying_key());
                let source = store.collection("source", policy.clone())?;
                Ok((source, policy))
            })
            .unwrap();
        let fixture = Self {
            _dir: dir,
            storage,
            source,
            policy,
            members: Cell::new(0),
        };
        for _ in 0..members {
            fixture.add_member();
        }
        fixture
    }

    fn add_member(&self) {
        let name = MEMBERS[self.members.get()];
        self.members.set(self.members.get() + 1);
        self.storage
            .with_store(|store, signer, _| {
                store.commit(self.source, signer, entity! { _ @ metadata::name: name })?;
                Ok(())
            })
            .unwrap();
    }

    /// Register the index of `identity` over the source: the descriptor
    /// only, as a query registers it.
    fn declare(&self, identity: WemmIdentity) -> Target {
        self.storage
            .with_store(|store, _, _| {
                let mapping = WemmIndex::<Embedding>::new(
                    [metadata::name.id()],
                    identity,
                    DEFAULT_WINDOW_CAP,
                )?;
                Ok(store.derive_with(self.source, mapping, self.policy.clone())?)
            })
            .unwrap()
    }

    /// A leaf of `target` signed by another host's key, as replication
    /// brings one: a record names the index, and this host's admission does
    /// not believe the leaf, so every foundation still needs one.
    fn name_by_a_record(&self, target: Target) {
        let other = SigningKey::from_bytes(&[0x77; 32]);
        self.storage
            .with_pile(|pile, _| {
                pile.insert(CollectionRecord::Derive(CollectionDerive::sign(
                    &other,
                    target.handle(),
                    SourceLocator::of([5; 32]),
                    Inline::new([6; 32]),
                )))?;
                Ok(())
            })
            .unwrap();
    }

    fn missing(&self, target: Target) -> usize {
        self.storage
            .with_store(|store, _, runtime| {
                underived(
                    &AcquiringReader::new(store.snapshot()?, runtime.clone()),
                    self.source,
                    target,
                )
            })
            .unwrap()
    }

    fn maintainer(
        &self,
        config: Config,
        unbindable: Vec<Id>,
    ) -> (Maintainer<TestBinder>, Rc<RefCell<Log>>) {
        let log = Rc::new(RefCell::new(Log::default()));
        let binder = TestBinder {
            log: log.clone(),
            unbindable,
            keeps_every_bind: false,
            bind_takes: Duration::ZERO,
            maintain_takes: Duration::ZERO,
        };
        (Maintainer::new(self.storage.clone(), binder, config), log)
    }
}

fn identity(root: u8, profile: Id) -> WemmIdentity {
    WemmIdentity {
        model_collection: Inline::new([1; 32]),
        model_root: Id::new([root; 16]).unwrap(),
        kernel_profile: profile,
    }
}

/// `models` listed exactly, each in its own collection.
fn config(max_bound: usize, models: &[WemmIdentity]) -> Config {
    Config {
        max_bound,
        models: models.iter().map(listed).collect(),
        retry: Duration::from_secs(3600),
    }
}

fn listed(model: &WemmIdentity) -> Listed {
    Listed {
        collection: Some(model.model_collection),
        root: model.model_root,
        profile: WemmProfile::of(model.kernel_profile).unwrap(),
    }
}

/// Two indexes over different roots and profiles: one pass finds both,
/// binds each one's own model once, and maintains each with it; the next
/// pass reuses both models.
#[test]
fn each_declared_index_is_maintained_with_the_model_its_descriptor_names() {
    let fixture = Fixture::new(3);
    let first_model = identity(2, GB10_NATIVE_BF16_PROFILE);
    let second_model = identity(3, GB10_NATIVE_W4A4_PROFILE);
    let first = fixture.declare(first_model);
    let second = fixture.declare(second_model);
    fixture.name_by_a_record(first);
    fixture.name_by_a_record(second);
    assert_eq!((fixture.missing(first), fixture.missing(second)), (3, 3));

    let (mut maintainer, log) =
        fixture.maintainer(config(2, &[first_model, second_model]), Vec::new());
    let lines = maintainer.pass().unwrap().lines;
    assert_eq!((fixture.missing(first), fixture.missing(second)), (0, 0));
    assert_eq!(log.borrow().binds, vec![first_model, second_model]);
    let mut maintained = log.borrow().maintained.clone();
    maintained.sort_by_key(|(_, index)| *index);
    let mut expected = vec![
        (first_model, first.handle()),
        (second_model, second.handle()),
    ];
    expected.sort_by_key(|(_, index)| *index);
    assert_eq!(maintained, expected, "{lines:#?}");
    assert!(
        lines
            .iter()
            .any(|line| line.contains("derived 3 foundation(s)")),
        "{lines:#?}"
    );

    fixture.add_member();
    maintainer.pass().unwrap().lines;
    assert_eq!((fixture.missing(first), fixture.missing(second)), (0, 0));
    assert_eq!(log.borrow().binds.len(), 2, "a bound model is bound again");
    // A pass with nothing to derive binds and logs nothing.
    assert!(maintainer.pass().unwrap().lines.is_empty());
}

/// The stand-in mirrors the runtime: under another model it derives
/// nothing, so a pass that handed an index the wrong model would leave it
/// missing.
#[test]
fn an_index_is_not_derived_under_another_model() {
    let fixture = Fixture::new(2);
    let model = identity(2, GB10_NATIVE_BF16_PROFILE);
    let index = fixture.declare(model);
    let (_, log) = fixture.maintainer(config(1, &[model]), Vec::new());
    let mut binder = TestBinder {
        log,
        unbindable: Vec::new(),
        keeps_every_bind: false,
        bind_takes: Duration::ZERO,
        maintain_takes: Duration::ZERO,
    };
    let other = identity(3, GB10_NATIVE_BF16_PROFILE);
    let derived = fixture
        .storage
        .with_store(|store, signer, runtime| {
            binder.maintain(&other, store, fixture.source, index, signer, runtime)
        })
        .unwrap();
    assert_eq!((derived.derived, derived.missing), (0, 2));
}

/// A model that cannot be bound is skipped with a line naming it and its
/// index; the other model's index is maintained; the bind is tried again
/// only after the retry interval.
#[test]
fn an_unbindable_model_is_skipped_with_a_line_and_the_others_are_maintained() {
    let fixture = Fixture::new(3);
    let good_model = identity(2, GB10_NATIVE_W4A4_VW4A16_PROFILE);
    let bad_model = identity(9, GB10_NATIVE_W4A4_VW4A16_PROFILE);
    let good = fixture.declare(good_model);
    let bad = fixture.declare(bad_model);
    fixture.name_by_a_record(good);
    fixture.name_by_a_record(bad);

    let (mut maintainer, log) = fixture.maintainer(
        config(1, &[good_model, bad_model]),
        vec![bad_model.model_root],
    );
    let lines = maintainer.pass().unwrap().lines;
    assert_eq!((fixture.missing(good), fixture.missing(bad)), (0, 3));
    let skip = lines
        .iter()
        .find(|line| line.contains("skip model"))
        .unwrap_or_else(|| panic!("no skip line: {lines:#?}"));
    assert!(
        skip.contains(&format!("{:X}", bad_model.model_root))
            && skip.contains(&collection_hex(bad.handle())[..16])
            && skip.contains("is not stated by the model collection"),
        "{skip}"
    );
    assert!(log
        .borrow()
        .maintained
        .iter()
        .all(|(model, index)| *model == good_model && *index == good.handle()));

    fixture.add_member();
    let lines = maintainer.pass().unwrap().lines;
    assert_eq!((fixture.missing(good), fixture.missing(bad)), (0, 4));
    let attempts = |log: &Log| log.binds.iter().filter(|m| **m == bad_model).count();
    assert_eq!(attempts(&log.borrow()), 1, "retried before the interval");
    assert!(!lines.iter().any(|line| line.contains("skip model")));
}

/// An index only registered is not found, so it is not maintained; once a
/// record names it (here a leaf replicated from another host) the next
/// pass of the same process maintains it, binding its model.
#[test]
fn an_index_named_after_start_is_maintained_on_the_next_pass() {
    let fixture = Fixture::new(2);
    let first_model = identity(2, GB10_NATIVE_BF16_PROFILE);
    let later_model = identity(4, GB10_NATIVE_W4A4_PROFILE);
    let first = fixture.declare(first_model);
    fixture.name_by_a_record(first);
    let (mut maintainer, log) =
        fixture.maintainer(config(2, &[first_model, later_model]), Vec::new());
    maintainer.pass().unwrap().lines;
    assert_eq!(fixture.missing(first), 0);

    let later = fixture.declare(later_model);
    maintainer.pass().unwrap().lines;
    assert_eq!(
        fixture.missing(later),
        2,
        "a registered-only index was found"
    );
    assert_eq!(log.borrow().binds, vec![first_model]);

    fixture.name_by_a_record(later);
    let lines = maintainer.pass().unwrap().lines;
    assert_eq!(fixture.missing(later), 0, "{lines:#?}");
    assert_eq!(log.borrow().binds, vec![first_model, later_model]);
    assert!(log
        .borrow()
        .maintained
        .contains(&(later_model, later.handle())));
}

/// With one model bound at a time, two models with work are served in
/// turn, the bound one first, and the release is said.
#[test]
fn one_bound_model_at_a_time_serves_every_model_in_turn() {
    let fixture = Fixture::new(2);
    let first_model = identity(2, GB10_NATIVE_BF16_PROFILE);
    let second_model = identity(3, GB10_NATIVE_BF16_PROFILE);
    let first = fixture.declare(first_model);
    let second = fixture.declare(second_model);
    fixture.name_by_a_record(first);
    fixture.name_by_a_record(second);
    let (mut maintainer, log) =
        fixture.maintainer(config(1, &[first_model, second_model]), Vec::new());
    let lines = maintainer.pass().unwrap().lines;
    assert_eq!((fixture.missing(first), fixture.missing(second)), (0, 0));
    assert_eq!(log.borrow().binds, vec![first_model, second_model]);
    assert_eq!(maintainer.bound(), vec![second_model]);
    assert!(
        lines
            .iter()
            .any(|line| line.contains("releasing model") && line.contains("2 model(s) have work")),
        "{lines:#?}"
    );

    fixture.add_member();
    maintainer.pass().unwrap().lines;
    assert_eq!((fixture.missing(first), fixture.missing(second)), (0, 0));
    assert_eq!(
        log.borrow().binds,
        vec![first_model, second_model, first_model],
        "the bound model is served before another is bound"
    );
}

/// Of two declared models only the listed one is bound and maintained;
/// each model is named once, the listed one as maintained and the other as
/// not listed, and the unlisted one's index is left as it is.
#[test]
fn only_a_listed_model_is_bound_and_maintained() {
    let fixture = Fixture::new(2);
    let kept_model = identity(2, GB10_NATIVE_W4A4_VW4A16_PROFILE);
    let retired_model = identity(3, GB10_NATIVE_W4A4_PROFILE);
    let kept = fixture.declare(kept_model);
    let retired = fixture.declare(retired_model);
    fixture.name_by_a_record(kept);
    fixture.name_by_a_record(retired);
    let only = Config::from_env_values(
        None,
        Some(OsStr::new(&format!(
            "{:X}/w4a4-vw4a16",
            kept_model.model_root
        ))),
        None,
        || panic!("the configured model is read only when nothing is listed"),
    )
    .unwrap();
    let (mut maintainer, log) = fixture.maintainer(only, Vec::new());
    let mut lines = maintainer.pass().unwrap().lines;
    fixture.add_member();
    lines.extend(maintainer.pass().unwrap().lines);
    assert_eq!(log.borrow().binds, vec![kept_model]);
    assert!(log
        .borrow()
        .maintained
        .iter()
        .all(|(model, index)| *model == kept_model && *index == kept.handle()));
    assert_eq!((fixture.missing(kept), fixture.missing(retired)), (0, 3));
    let count = |text: &str| lines.iter().filter(|line| line.contains(text)).count();
    assert_eq!(count("is not listed"), 1, "{lines:#?}");
    assert_eq!(count("is listed; maintaining"), 1, "{lines:#?}");
    let not_listed = lines
        .iter()
        .find(|line| line.contains("is not listed"))
        .unwrap();
    assert!(
        not_listed.contains(&format!("{:X}", retired_model.model_root))
            && not_listed.contains(&collection_hex(retired.handle())[..16]),
        "{not_listed}"
    );
}

/// A model listed with its collection does not cover the same root and
/// profile in another collection.
#[test]
fn a_collection_qualified_model_covers_only_its_own_collection() {
    let model = identity(2, GB10_NATIVE_BF16_PROFILE);
    let elsewhere = WemmIdentity {
        model_collection: Inline::new([2; 32]),
        ..model
    };
    let qualified = listed(&model);
    let unqualified = Listed {
        collection: None,
        ..qualified
    };
    assert!(qualified.covers(&model) && !qualified.covers(&elsewhere));
    assert!(unqualified.covers(&model) && unqualified.covers(&elsewhere));
    let other_profile = WemmIdentity {
        kernel_profile: GB10_NATIVE_W4A4_PROFILE,
        ..model
    };
    assert!(!unqualified.covers(&other_profile));
}

/// Unset, the list is the one model the single-model mode is configured
/// with; with nothing listed and nothing configured the mode refuses to
/// start, and so does a list with no model in it.
#[test]
fn with_no_model_listed_or_configured_the_declared_mode_refuses() {
    let configured = listed(&identity(2, GB10_NATIVE_W4A4_VW4A16_PROFILE));
    let defaults = Config::from_env_values(None, None, None, || Ok(Some(configured))).unwrap();
    assert_eq!(
        defaults,
        Config {
            max_bound: 1,
            models: vec![configured],
            retry: DEFAULT_BIND_RETRY,
        }
    );
    let error = Config::from_env_values(None, None, None, || Ok(None))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("none is") && error.contains(MODELS_ENV) && error.contains("WEMM_ROOT"),
        "{error}"
    );
    for empty in ["", " , ,"] {
        assert!(
            Config::from_env_values(None, Some(OsStr::new(empty)), None, || Ok(Some(configured)))
                .is_err()
        );
    }
}

#[test]
fn the_configuration_is_exactly_what_is_written() {
    let root = "25A3305015AC1B827E7856DE37DBF723";
    let other = "E1A7DFB9D59FD338DB1C89F96E9672E1";
    let collection = "a03d075c3cc31e5644b80dda054d0d59ec5b6c898cae1305143de055b0fb75e7";
    let written = Config::from_env_values(
        Some(OsStr::new("2")),
        Some(OsStr::new(&format!(
            "{root}/w4a4-vw4a16, {collection}/{other}/w4a4,"
        ))),
        Some(OsStr::new("60")),
        || panic!("not read"),
    )
    .unwrap();
    assert_eq!(written.max_bound, 2);
    assert_eq!(written.retry, Duration::from_secs(60));
    assert_eq!(
        written.models,
        vec![
            Listed {
                collection: None,
                root: Id::from_hex(root).unwrap(),
                profile: WemmProfile::W4a4Vw4a16,
            },
            Listed {
                collection: Some(
                    mary::model_collection::parse_collection_handle(collection).unwrap()
                ),
                root: Id::from_hex(other).unwrap(),
                profile: WemmProfile::W4a4,
            },
        ]
    );
    let some = format!("{root}/bf16");
    for (max_bound, models, retry) in [
        (Some("0"), Some(some.as_str()), None),
        (Some("one"), Some(some.as_str()), None),
        (None, Some(root), None),
        (None, Some("25A3/bf16"), None),
        (None, Some("25A3305015AC1B827E7856DE37DBF723/fp4"), None),
        (
            None,
            Some("abc/25A3305015AC1B827E7856DE37DBF723/bf16"),
            None,
        ),
        (None, Some(some.as_str()), Some("soon")),
    ] {
        assert!(
            Config::from_env_values(
                max_bound.map(OsStr::new),
                models.map(OsStr::new),
                retry.map(OsStr::new),
                || Ok(None)
            )
            .is_err(),
            "{max_bound:?} {models:?} {retry:?} accepted"
        );
    }
}

/// A profile this build does not run is refused before the pile is opened
/// (the pile here does not exist), and so before any device work.
#[test]
fn an_unknown_profile_is_refused_before_anything_is_opened() {
    let dir = tempfile::tempdir().unwrap();
    let error = match super::super::Session::bind_identity(
        &dir.path().join("absent.pile"),
        identity(2, Id::new([8; 16]).unwrap()),
        mary::nn::cuda_bf16_alias::WeightResidence::PileAliased,
        0,
    ) {
        Ok(_) => panic!("bound an unknown profile"),
        Err(error) => format!("{error:#}"),
    };
    assert!(error.contains("is not one this build runs"), "{error}");
}

/// Discovery reads descriptors only from what is here. An index that a
/// record names but whose descriptor is not here is passed over, even when
/// the reader maintenance acquires through holds the descriptor; with the
/// descriptor here it is found, and its backlog read.
#[test]
fn discovery_reads_descriptors_only_from_what_is_here() {
    use triblespace::core::repo::memoryrepo::MemoryRepo;
    use triblespace::core::repo::SnapshotSource;
    let signer = SigningKey::from_bytes(&[9; 32]);
    let policy = crate::collection_names::private_policy(signer.verifying_key());
    let model = identity(2, GB10_NATIVE_BF16_PROFILE);
    let mut full = MemoryRepo::default();
    let source = full.collection("source", policy.clone()).unwrap();
    let mapping =
        WemmIndex::<Embedding>::new([metadata::name.id()], model, DEFAULT_WINDOW_CAP).unwrap();
    let target: Target = full.derive_with(source, mapping, policy).unwrap();
    let record = || {
        CollectionRecord::Derive(CollectionDerive::sign(
            &signer,
            target.handle(),
            SourceLocator::of([5; 32]),
            Inline::new([6; 32]),
        ))
    };
    full.insert(record()).unwrap();
    let mut records_only = MemoryRepo::default();
    records_only.insert(record()).unwrap();
    let config = config(1, &[model]);

    let here = discover(
        &full.snapshot().unwrap(),
        &full.snapshot().unwrap(),
        &config,
    )
    .unwrap();
    assert_eq!(here.len(), 1);
    assert_eq!(here[0].0.index, target.handle());
    assert!(matches!(here[0].1, Some(Ok(0))), "{:?}", here[0].1);

    let absent = discover(
        &records_only.snapshot().unwrap(),
        &full.snapshot().unwrap(),
        &config,
    )
    .unwrap();
    assert!(
        absent.is_empty(),
        "a descriptor not here was read: {absent:?}"
    );
}

/// A failed bind names when it is due again; a pass before then does not
/// bind, and the pass at that moment binds again although the pile has not
/// grown (the daemon loop runs that pass: pile_watch's deadline tests).
#[test]
fn a_failed_bind_names_its_retry_and_is_retried_then_without_growth() {
    let fixture = Fixture::new(2);
    let bad_model = identity(9, GB10_NATIVE_W4A4_VW4A16_PROFILE);
    let bad = fixture.declare(bad_model);
    fixture.name_by_a_record(bad);
    let retry = Duration::from_secs(1);
    let (mut maintainer, log) = fixture.maintainer(
        Config {
            retry,
            ..config(1, &[bad_model])
        },
        vec![bad_model.model_root],
    );
    let attempts = |log: &Log| log.binds.len();
    let before = Instant::now();
    let due = maintainer
        .pass()
        .unwrap()
        .retry_at
        .expect("a failed bind names when it is retried");
    assert!(due > before && due <= Instant::now() + retry, "{due:?}");
    let early = maintainer.pass().unwrap();
    assert_eq!(early.retry_at, Some(due));
    assert_eq!(attempts(&log.borrow()), 1);
    std::thread::sleep(due.saturating_duration_since(Instant::now()));
    let retried = maintainer.pass().unwrap();
    assert_eq!(attempts(&log.borrow()), 2);
    assert!(retried.retry_at.is_some_and(|next| next > due));
    assert_eq!(fixture.missing(bad), 2);
}

/// A binder whose binds stay until the process exits (weights aliased from
/// the pile) binds one model per process: a second model with work is
/// refused with a line, never bound in turn.
#[test]
fn an_aliasing_binder_binds_one_model_per_process() {
    let fixture = Fixture::new(2);
    let first_model = identity(2, GB10_NATIVE_BF16_PROFILE);
    let second_model = identity(3, GB10_NATIVE_BF16_PROFILE);
    let first = fixture.declare(first_model);
    let second = fixture.declare(second_model);
    fixture.name_by_a_record(first);
    fixture.name_by_a_record(second);
    let log = Rc::new(RefCell::new(Log::default()));
    let binder = TestBinder {
        log: log.clone(),
        unbindable: Vec::new(),
        keeps_every_bind: true,
        bind_takes: Duration::ZERO,
        maintain_takes: Duration::ZERO,
    };
    let mut maintainer = Maintainer::new(
        fixture.storage.clone(),
        binder,
        config(1, &[first_model, second_model]),
    );
    let lines = maintainer.pass().unwrap().lines;
    assert_eq!(log.borrow().binds, vec![first_model]);
    assert_eq!(maintainer.bound(), vec![first_model]);
    assert_eq!((fixture.missing(first), fixture.missing(second)), (0, 2));
    assert!(
        lines
            .iter()
            .any(|line| line.contains("not binding model") && line.contains("WEMM_COPY_WEIGHTS=1")),
        "{lines:#?}"
    );
}

/// Several listed models refuse to start with aliased weights and start
/// with device copies, saying the release between binds is unverified; one
/// model runs either way, as today.
#[test]
fn several_models_need_device_copies_and_say_what_is_unverified() {
    let one = config(1, &[identity(2, GB10_NATIVE_BF16_PROFILE)]);
    assert_eq!(one.check_residence(true).unwrap(), None);
    assert_eq!(one.check_residence(false).unwrap(), None);
    let several = config(
        1,
        &[
            identity(2, GB10_NATIVE_BF16_PROFILE),
            identity(3, GB10_NATIVE_W4A4_PROFILE),
        ],
    );
    let error = several.check_residence(true).unwrap_err().to_string();
    assert!(error.contains("WEMM_COPY_WEIGHTS=1"), "{error}");
    let note = several.check_residence(false).unwrap().unwrap();
    assert!(note.contains("NOT verified"), "{note}");
}

fn slow_binder(
    log: &Rc<RefCell<Log>>,
    unbindable: Vec<Id>,
    bind_takes: Duration,
    maintain_takes: Duration,
) -> TestBinder {
    TestBinder {
        log: log.clone(),
        unbindable,
        keeps_every_bind: false,
        bind_takes,
        maintain_takes,
    }
}

/// A bind that fails after taking longer than the retry interval is still
/// due again: the interval counts from when the attempt ended, so the pass
/// names a moment after it, and the attempt then happens on a quiet pile.
#[test]
fn a_bind_that_fails_slower_than_the_retry_interval_is_still_retried() {
    let fixture = Fixture::new(2);
    let bad_model = identity(9, GB10_NATIVE_W4A4_VW4A16_PROFILE);
    let bad = fixture.declare(bad_model);
    fixture.name_by_a_record(bad);
    let retry = Duration::from_secs(1);
    let log = Rc::new(RefCell::new(Log::default()));
    let mut maintainer = Maintainer::new(
        fixture.storage.clone(),
        slow_binder(
            &log,
            vec![bad_model.model_root],
            Duration::from_millis(1300),
            Duration::ZERO,
        ),
        Config {
            retry,
            ..config(1, &[bad_model])
        },
    );
    let pass = maintainer.pass().unwrap();
    let ended = Instant::now();
    let due = pass.retry_at.expect("the failed bind has no next attempt");
    assert!(
        due + Duration::from_millis(100) >= ended + retry,
        "{due:?} {ended:?}"
    );
    std::thread::sleep(due.saturating_duration_since(Instant::now()));
    maintainer.pass().unwrap();
    assert_eq!(log.borrow().binds.len(), 2);
}

/// A failed model whose next attempt came due while a later model was
/// being maintained is still named: the pass returns that moment although
/// it has passed, so the daemon runs the next pass at once.
#[test]
fn a_retry_that_came_due_during_the_pass_is_still_named() {
    let fixture = Fixture::new(2);
    let bad_model = identity(1, GB10_NATIVE_W4A4_VW4A16_PROFILE);
    let slow_model = identity(2, GB10_NATIVE_W4A4_VW4A16_PROFILE);
    let bad = fixture.declare(bad_model);
    let slow = fixture.declare(slow_model);
    fixture.name_by_a_record(bad);
    fixture.name_by_a_record(slow);
    let log = Rc::new(RefCell::new(Log::default()));
    let mut maintainer = Maintainer::new(
        fixture.storage.clone(),
        slow_binder(
            &log,
            vec![bad_model.model_root],
            Duration::ZERO,
            Duration::from_millis(1300),
        ),
        Config {
            retry: Duration::from_secs(1),
            ..config(2, &[bad_model, slow_model])
        },
    );
    let pass = maintainer.pass().unwrap();
    assert_eq!(fixture.missing(slow), 0);
    let due = pass.retry_at.expect("the pending retry was dropped");
    assert!(due <= Instant::now(), "{due:?}");
    maintainer.pass().unwrap();
    assert_eq!(
        log.borrow()
            .binds
            .iter()
            .filter(|m| **m == bad_model)
            .count(),
        2
    );
}

/// With no retry interval a failing bind is still not tried faster than
/// the floor (one second here, the poll interval in the daemon): an
/// immediate pass does not try it again, and the next attempt is named
/// at least the floor after the failure.
#[test]
fn a_zero_retry_interval_does_not_retry_faster_than_the_floor() {
    let fixture = Fixture::new(2);
    let bad_model = identity(9, GB10_NATIVE_W4A4_VW4A16_PROFILE);
    let bad = fixture.declare(bad_model);
    fixture.name_by_a_record(bad);
    let (mut maintainer, log) = fixture.maintainer(
        Config {
            retry: Duration::ZERO,
            ..config(1, &[bad_model])
        },
        vec![bad_model.model_root],
    );
    let first = maintainer.pass().unwrap();
    let failed = Instant::now();
    for _ in 0..5 {
        maintainer.pass().unwrap();
    }
    assert_eq!(log.borrow().binds.len(), 1, "retried faster than the floor");
    let due = first.retry_at.expect("a failed bind is due again");
    assert!(
        due + Duration::from_millis(100) >= failed + Duration::from_secs(1),
        "{due:?}"
    );
}

/// The daemon floors the retry interval at its poll interval, or at one
/// second when it polls continuously; a longer interval is kept.
#[test]
fn the_daemon_floors_the_retry_interval_at_its_poll() {
    let zero = Config {
        retry: Duration::ZERO,
        ..config(1, &[identity(2, GB10_NATIVE_BF16_PROFILE)])
    };
    assert_eq!(
        zero.clone().floor_retry(Duration::from_secs(60)).retry,
        Duration::from_secs(60)
    );
    assert_eq!(zero.floor_retry(Duration::ZERO).retry, MIN_BIND_RETRY);
    let long = config(1, &[identity(2, GB10_NATIVE_BF16_PROFILE)]);
    assert_eq!(
        long.floor_retry(Duration::from_secs(60)).retry,
        Duration::from_secs(3600)
    );
}
