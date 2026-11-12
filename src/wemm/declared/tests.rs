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
}

impl Binder for TestBinder {
    type Model = WemmIdentity;

    fn bind(&mut self, identity: WemmIdentity) -> Result<WemmIdentity> {
        self.log.borrow_mut().binds.push(identity);
        anyhow::ensure!(
            !self.unbindable.contains(&identity.model_root),
            "WEMM_ROOT {:X} is not stated by the model collection",
            identity.model_root
        );
        Ok(identity)
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

fn config(max_bound: usize) -> Config {
    Config {
        max_bound,
        skip: Vec::new(),
        retry: Duration::from_secs(3600),
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

    let (mut maintainer, log) = fixture.maintainer(config(2), Vec::new());
    let lines = maintainer.pass().unwrap();
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
    maintainer.pass().unwrap();
    assert_eq!((fixture.missing(first), fixture.missing(second)), (0, 0));
    assert_eq!(log.borrow().binds.len(), 2, "a bound model is bound again");
    // A pass with nothing to derive binds and logs nothing.
    assert!(maintainer.pass().unwrap().is_empty());
}

/// The stand-in mirrors the runtime: under another model it derives
/// nothing, so a pass that handed an index the wrong model would leave it
/// missing.
#[test]
fn an_index_is_not_derived_under_another_model() {
    let fixture = Fixture::new(2);
    let model = identity(2, GB10_NATIVE_BF16_PROFILE);
    let index = fixture.declare(model);
    let (_, log) = fixture.maintainer(config(1), Vec::new());
    let mut binder = TestBinder {
        log,
        unbindable: Vec::new(),
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

    let (mut maintainer, log) = fixture.maintainer(config(1), vec![bad_model.model_root]);
    let lines = maintainer.pass().unwrap();
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
    let lines = maintainer.pass().unwrap();
    assert_eq!((fixture.missing(good), fixture.missing(bad)), (0, 4));
    let attempts = |log: &Log| log.binds.iter().filter(|m| **m == bad_model).count();
    assert_eq!(attempts(&log.borrow()), 1, "retried before the interval");
    assert!(!lines.iter().any(|line| line.contains("skip model")));

    // With no interval it is tried on every pass, and still never replaced.
    let (mut eager, log) = fixture.maintainer(
        Config {
            retry: Duration::ZERO,
            ..config(1)
        },
        vec![bad_model.model_root],
    );
    eager.pass().unwrap();
    eager.pass().unwrap();
    assert_eq!(attempts(&log.borrow()), 2);
    assert_eq!(fixture.missing(bad), 4);
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
    let (mut maintainer, log) = fixture.maintainer(config(2), Vec::new());
    maintainer.pass().unwrap();
    assert_eq!(fixture.missing(first), 0);

    let later = fixture.declare(later_model);
    maintainer.pass().unwrap();
    assert_eq!(
        fixture.missing(later),
        2,
        "a registered-only index was found"
    );
    assert_eq!(log.borrow().binds, vec![first_model]);

    fixture.name_by_a_record(later);
    let lines = maintainer.pass().unwrap();
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
    let (mut maintainer, log) = fixture.maintainer(config(1), Vec::new());
    let lines = maintainer.pass().unwrap();
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
    maintainer.pass().unwrap();
    assert_eq!((fixture.missing(first), fixture.missing(second)), (0, 0));
    assert_eq!(
        log.borrow().binds,
        vec![first_model, second_model, first_model],
        "the bound model is served before another is bound"
    );
}

/// A model on the skip list is never bound; it is named once.
#[test]
fn a_skipped_model_is_never_bound_and_is_named_once() {
    let fixture = Fixture::new(2);
    let kept_model = identity(2, GB10_NATIVE_W4A4_VW4A16_PROFILE);
    let retired_model = identity(3, GB10_NATIVE_W4A4_PROFILE);
    let kept = fixture.declare(kept_model);
    let retired = fixture.declare(retired_model);
    fixture.name_by_a_record(kept);
    fixture.name_by_a_record(retired);
    let skip = Config::from_env_values(
        None,
        Some(OsStr::new(&format!("{:X}/w4a4", retired_model.model_root))),
        None,
    )
    .unwrap();
    let (mut maintainer, log) = fixture.maintainer(skip, Vec::new());
    let mut lines = maintainer.pass().unwrap();
    fixture.add_member();
    lines.extend(maintainer.pass().unwrap());
    assert_eq!(log.borrow().binds, vec![kept_model]);
    assert_eq!((fixture.missing(kept), fixture.missing(retired)), (0, 3));
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.contains("not binding model"))
            .count(),
        1,
        "{lines:#?}"
    );
}

#[test]
fn the_configuration_is_its_defaults_or_exactly_what_is_written() {
    let defaults = Config::from_env_values(None, None, None).unwrap();
    assert_eq!(defaults, config(1).with_retry(DEFAULT_BIND_RETRY));
    let root = "25A3305015AC1B827E7856DE37DBF723";
    let other = "E1A7DFB9D59FD338DB1C89F96E9672E1";
    let written = Config::from_env_values(
        Some(OsStr::new("2")),
        Some(OsStr::new(&format!("{root}, {other}/w4a4,"))),
        Some(OsStr::new("60")),
    )
    .unwrap();
    assert_eq!(written.max_bound, 2);
    assert_eq!(written.retry, Duration::from_secs(60));
    assert_eq!(
        written.skip,
        vec![
            Skip {
                root: Id::from_hex(root).unwrap(),
                profile: None,
            },
            Skip {
                root: Id::from_hex(other).unwrap(),
                profile: Some(WemmProfile::W4a4),
            },
        ]
    );
    for (max_bound, skip, retry) in [
        (Some("0"), None, None),
        (Some("one"), None, None),
        (None, Some("25A3"), None),
        (None, Some("25A3305015AC1B827E7856DE37DBF723/fp4"), None),
        (None, None, Some("soon")),
    ] {
        assert!(
            Config::from_env_values(
                max_bound.map(OsStr::new),
                skip.map(OsStr::new),
                retry.map(OsStr::new)
            )
            .is_err(),
            "{max_bound:?} {skip:?} {retry:?} accepted"
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

impl Config {
    fn with_retry(mut self, retry: Duration) -> Self {
        self.retry = retry;
        self
    }
}
