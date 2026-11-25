//! The colony's rule environment: positive sensors, text rules, one driver.
//!
//! JP's focus lane of 17 Nov 2026 (goals 8e066e0c): a living expert system
//! that keeps inferring and triggers things like spawning zooids on idle goals.
//! The shape is a nervous system:
//!
//! - **Sensors** turn the world into positive, timed observations. The rule
//!   language has no absence, clock or numbers, so a sensor that sees nothing
//!   publishes nothing. One tick is one entity holding everything observed at
//!   that moment ([`tick_fragment`]).
//! - **Rules** ([`program`]) are text in `src/rules/*.rules`, positive only,
//!   and conclude requests whose identity derives from the conclusion alone.
//! - **The driver** is triblespace's `CollectionDriver`, stepped by [`settle`].
//! - **Effectors** act on conclusions. A conclusion is a request, never
//!   permission; an effector revalidates before acting. There is none here:
//!   the ignite effector that started a run for a concluded goal was removed
//!   on 2026-11-24, and the nudge never had one. Conclusions are published
//!   and nothing acts on them.
//!
//! The driver runs whatever the pile's current reading selects. The text in
//! `src/rules/*.rules` is a seed, published by `rules tick` only when no
//! reading exists; a revised rule is a new revision under a new reading.
//!
//! Requests are tick-free: `run_for(goal)` is one enduring request per goal
//! (ignite ignites a goal once), and `nudge_after(last asking)` is one
//! aggregate reminder per quiet period. Neither is fresh when it is read.

pub mod cli;

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error as StdError;

use anyhow::{Context, Result};
use triblespace::core::collection::observed_store::ObservedStore;
use triblespace::core::collection::{Collection, CollectionStore};
use triblespace::core::metadata;
use triblespace::core::reactive::collection::{schema as environment, AdvanceError};
use triblespace::core::reactive::driver::{CollectionDriver, DriverError, Outcome};
use triblespace::core::reactive::text;
use triblespace::core::repo::{BlobStorePut, SnapshotSource, StorageFlush, StoreRead};
use triblespace::prelude::*;

use crate::goals::GoalState;
use crate::quota::operations::QuotaView;
use crate::quota::{self, Pace};
use crate::schemas::quota::PROVIDER_CLAUDE;
use crate::schemas::rules::{
    rules, ASK_JP_REVISION, ENVIRONMENT_ANCHOR, IGNITE_REVISION, KIND_TICK,
};

pub type IntervalValue = Inline<inlineencodings::NsTAIInterval>;

/// Tags that keep a goal out of ignition, as `ignite.sh` has them. A
/// `horizon` goal is moved by its owner in idle moments, never dispatched.
pub const IGNITE_SKIP: [&str; 10] = [
    "horizon",
    "parking",
    "later",
    "idea",
    "hobby",
    "play",
    "christmas",
    "dreams",
    "bujo",
    "ask-jp",
];

/// The weekly window, as `ignite.sh` selects it.
pub const WEEK_SECS: i64 = 604_800;

/// How long after an asking JP is not asked again (`ask-jp-due.sh`).
pub const ASK_QUIET_SECS: i64 = 86_400;

const IGNITE_TEXT: &str = include_str!("rules/ignite.rules");
const ASK_JP_TEXT: &str = include_str!("rules/ask_jp.rules");

/// Everything one sensor pass observed. Absence is an empty list or `None`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sensed {
    pub eligible: Vec<Id>,
    /// The fresh weekly Claude quota observation that was under or on pace.
    pub quota_ok: Option<Id>,
    pub ask_open: Vec<Id>,
    /// The last asking (Unix seconds, 0 when never) when it is over 24 h ago.
    pub ask_quiet_since: Option<i64>,
}

/// Goals `ignite.sh` would consider: todo, tagged `#owner`, none of
/// [`IGNITE_SKIP`], not ignited before. Tags are the goal's tag facts only:
/// words in a title that look like tags are not tags.
pub fn eligible_goals(states: &[GoalState], owner: &str, ignited: &BTreeSet<Id>) -> Vec<Id> {
    let mut goals: Vec<Id> = states
        .iter()
        .filter(|state| state.status.as_deref() == Some("todo"))
        .filter(|state| state.tags.iter().any(|tag| tag == owner))
        .filter(|state| {
            !state
                .tags
                .iter()
                .any(|tag| IGNITE_SKIP.contains(&tag.as_str()))
        })
        .filter(|state| !ignited.contains(&state.goal))
        .map(|state| state.goal)
        .collect();
    goals.sort();
    goals
}

/// The ignited list: one goal id per line, as `ignite.sh` writes it. Read
/// only: nothing here appends to it.
/// A missing file is an empty list; a line that is not an id is ignored.
pub fn read_ignited(path: &std::path::Path) -> Result<BTreeSet<Id>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    Ok(text.split_whitespace().filter_map(Id::from_hex).collect())
}

/// Open `#ask-jp` goals: tagged, with a current status that is not done.
pub fn open_asks(states: &[GoalState]) -> Vec<Id> {
    let mut goals: Vec<Id> = states
        .iter()
        .filter(|state| state.tags.iter().any(|tag| tag == "ask-jp"))
        .filter(|state| {
            state
                .status
                .as_deref()
                .is_some_and(|status| status != "done")
        })
        .map(|state| state.goal)
        .collect();
    goals.sort();
    goals
}

/// The first fresh weekly Claude observation, if it is under or on pace.
/// A stale or ambiguous reading yields nothing: capacity is then unknown,
/// and the sensor says nothing rather than something it did not see.
pub fn quota_ok(view: &QuotaView) -> Option<Id> {
    let group = view.groups.iter().find(|group| {
        group.provider == PROVIDER_CLAUDE
            && group.window_secs == WEEK_SECS
            && (view.now - group.observed) / 1_000_000_000 <= quota::STALE_AFTER_SECONDS as i128
    })?;
    let ([used], [resets]) = (group.used_percent.as_slice(), group.resets_at.as_slice()) else {
        return None;
    };
    let window = i128::from(group.window_secs) * 1_000_000_000;
    match quota::pace(view.now, *used, *resets, window) {
        Pace::Under | Pace::On => Some(group.event),
        Pace::Over | Pace::Reset => None,
    }
}

/// The last asking when it lies more than [`ASK_QUIET_SECS`] in the past.
pub fn ask_quiet_since(last_asked: Option<i64>, now: i64) -> Option<i64> {
    match last_asked {
        Some(asked) if now - asked < ASK_QUIET_SECS => None,
        other => Some(other.unwrap_or(0)),
    }
}

/// One tick: its kind, its observation time, and every positive observation.
/// Intrinsically identified, so the tick is what it saw.
pub fn tick_fragment(at: IntervalValue, sensed: &Sensed) -> Result<Fragment> {
    let quiet = sensed.ask_quiet_since.map(quota::unix_point).transpose()?;
    Ok(entity! { _ @
        metadata::tag: &KIND_TICK,
        metadata::created_at: at,
        rules::eligible*: sensed.eligible.iter().copied(),
        rules::quota_ok?: sensed.quota_ok,
        rules::ask_open*: sensed.ask_open.iter().copied(),
        rules::ask_quiet_since?: quiet,
    })
}

/// Both rule revisions, parsed from their text with the attribute aliases
/// bound to `attribute.id()`, plus the one reading that selects them.
pub fn program() -> Result<Fragment> {
    let attributes = [
        ("eligible", rules::eligible.id()),
        ("quota_ok", rules::quota_ok.id()),
        ("ask_open", rules::ask_open.id()),
        ("ask_quiet_since", rules::ask_quiet_since.id()),
        ("run_for", rules::run_for.id()),
        ("requested_in", rules::requested_in.id()),
        ("nudge_after", rules::nudge_after.id()),
        ("nudge_about", rules::nudge_about.id()),
        ("nudge_in", rules::nudge_in.id()),
    ];
    let header: String = attributes
        .iter()
        .map(|(name, id)| format!("alias {name} = id\"{id:x}\";\n"))
        .collect();
    let mut fragment = Fragment::empty();
    for (name, body) in [("ignite", IGNITE_TEXT), ("ask_jp", ASK_JP_TEXT)] {
        let source = format!("{header}{body}");
        fragment += text::parse(source.into_bytes().into())
            .with_context(|| format!("parse src/rules/{name}.rules"))?;
    }
    fragment += entity! { _ @
        environment::reading_of: ENVIRONMENT_ANCHOR,
        environment::selects*: [IGNITE_REVISION, ASK_JP_REVISION],
    };
    Ok(fragment)
}

/// Run requests: request entity to goal.
pub fn run_requests<P: TriblePattern>(facts: &P) -> BTreeMap<Id, Id> {
    find!((request: Id, goal: Id), pattern!(facts, [{ ?request @ rules::run_for: ?goal }]))
        .collect()
}

/// Run requests supported by one tick.
pub fn run_requests_in<P: TriblePattern>(facts: &P, tick: Id) -> BTreeMap<Id, Id> {
    find!(
        (request: Id, goal: Id),
        pattern!(facts, [{ ?request @ rules::run_for: ?goal, rules::requested_in: &tick }])
    )
    .collect()
}

/// Nudge requests supported by one tick, each with the questions it carries.
pub fn nudges_in<P: TriblePattern>(facts: &P, tick: Id) -> BTreeMap<Id, BTreeSet<Id>> {
    let mut nudges = BTreeMap::<Id, BTreeSet<Id>>::new();
    for (nudge, question) in find!(
        (nudge: Id, question: Id),
        pattern!(facts, [{ ?nudge @ rules::nudge_in: &tick, rules::nudge_about: ?question }])
    ) {
        nudges.entry(nudge).or_default().insert(question);
    }
    nudges
}

/// What settling a driver did.
#[derive(Debug, Default)]
pub struct Settled {
    pub polls: usize,
    pub firings: usize,
    pub commits: usize,
    /// The last poll's outcome, rendered.
    pub outcome: String,
    /// Whether the last poll reached `Unchanged` (nothing left to do).
    pub settled: bool,
}

/// Poll until the driver reports `Unchanged`, at most `max_polls` times.
/// Each poll is bounded by the driver's own [`Bounds`]; a `Blocked` or
/// `Unresolved` outcome stops early and is reported, not retried here.
///
/// [`Bounds`]: triblespace::core::reactive::driver::Bounds
pub fn settle<S, T>(
    driver: &mut CollectionDriver<T>,
    store: &mut S,
    max_polls: usize,
) -> std::result::Result<Settled, DriverError>
where
    S: SnapshotSource<Snapshot = T> + BlobStorePut + CollectionStore + StorageFlush,
    T: StoreRead + triblespace::core::repo::StoreSnapshot,
    AdvanceError<
        <ObservedStore<T> as BlobStoreGet>::GetError<
            <View<str> as TryFromBlob<blobencodings::UTF8String>>::Error,
        >,
    >: StdError + Send + Sync + 'static,
{
    let mut settled = Settled::default();
    while settled.polls < max_polls {
        let step = driver.poll(store)?;
        settled.polls += 1;
        settled.firings += step.firings;
        settled.commits += step.commits.len();
        settled.outcome = match &step.outcome {
            Outcome::Unchanged => "unchanged".to_owned(),
            Outcome::Quiescent => "quiescent".to_owned(),
            Outcome::Bounded(bound) => format!("bounded by {bound:?}"),
            Outcome::Unresolved(reading) => format!("unresolved reading {reading:?}"),
            Outcome::Blocked(cause) => format!("blocked: {cause}"),
        };
        match step.outcome {
            Outcome::Unchanged => {
                settled.settled = true;
                break;
            }
            Outcome::Quiescent | Outcome::Bounded(_) => {}
            Outcome::Unresolved(_) | Outcome::Blocked(_) => break,
        }
    }
    Ok(settled)
}

/// The root collection type the driver observes.
pub type Root = Collection<blobencodings::SimpleArchive>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quota::operations::QuotaGroup;
    use ed25519_dalek::SigningKey;
    use triblespace::core::collection::{AdmissionPolicy, CollectionPolicy, CollectionStoreExt};
    use triblespace::core::reactive::driver::Bounds;
    use triblespace::core::repo::memoryrepo::{MemoryRepo, MemoryRepoSnapshot};

    const NS: i128 = 1_000_000_000;

    fn group(observed_secs_ago: i128, used: f64, elapsed_fraction: f64) -> QuotaGroup {
        let now = 1_000_000 * NS;
        let window = WEEK_SECS as i128 * NS;
        let resets = now + ((1.0 - elapsed_fraction) * window as f64) as i128;
        QuotaGroup {
            event: *genid(),
            provider: PROVIDER_CLAUDE,
            provider_name: Some("claude".to_owned()),
            account: "a".to_owned(),
            window_secs: WEEK_SECS,
            bucket: None,
            observed: now - observed_secs_ago * NS,
            used_percent: vec![used],
            resets_at: vec![resets],
            host: vec![],
        }
    }

    fn view(groups: Vec<QuotaGroup>) -> QuotaView {
        QuotaView {
            now: 1_000_000 * NS,
            examined: groups.len(),
            groups,
            status_examined: 0,
            statuses: vec![],
            status_error: None,
        }
    }

    #[test]
    fn a_stale_quota_reading_publishes_nothing() {
        // Under pace, but read seven hours ago: capacity is unknown.
        let stale = group(7 * 3_600, 10.0, 0.5);
        assert_eq!(quota_ok(&view(vec![stale])), None);
        let fresh = group(60, 10.0, 0.5);
        let event = fresh.event;
        assert_eq!(quota_ok(&view(vec![fresh])), Some(event));
        assert_eq!(
            quota_ok(&view(vec![group(60, 90.0, 0.5)])),
            None,
            "over pace"
        );
        // No quota observation at all in the tick, so no run request.
        let sensed = Sensed {
            eligible: vec![*genid()],
            quota_ok: quota_ok(&view(vec![group(7 * 3_600, 10.0, 0.5)])),
            ..Sensed::default()
        };
        let tick = tick_fragment(quota::unix_point(1).unwrap(), &sensed).unwrap();
        assert!(
            find!(q: Id, pattern!(tick.facts(), [{ _?t @ rules::quota_ok: ?q }]))
                .next()
                .is_none()
        );
    }

    #[test]
    fn eligibility_and_quiet_follow_the_scripts() {
        let state = |status: &str, tags: &[&str]| GoalState {
            goal: *genid(),
            status: Some(status.to_owned()),
            tags: tags.iter().map(|t| t.to_string()).collect(),
        };
        let ready = state("todo", &["focus", "zooid-a"]);
        let parked = state("todo", &["zooid-a", "parking"]);
        let horizon = state("todo", &["zooid-a", "horizon"]);
        let other = state("todo", &["zooid-b"]);
        let doing = state("doing", &["zooid-a"]);
        let ignited_goal = state("todo", &["zooid-a"]);
        let ask = state("blocked", &["ask-jp", "zooid-a"]);
        let answered = state("done", &["ask-jp"]);
        let states = vec![
            ready.clone(),
            parked,
            horizon,
            other,
            doing,
            ignited_goal.clone(),
            ask.clone(),
            answered,
        ];
        let ignited = BTreeSet::from([ignited_goal.goal]);
        assert_eq!(
            eligible_goals(&states, "zooid-a", &ignited),
            vec![ready.goal]
        );
        assert_eq!(open_asks(&states), vec![ask.goal]);
        assert_eq!(ask_quiet_since(None, 100), Some(0));
        assert_eq!(ask_quiet_since(Some(100), 100 + ASK_QUIET_SECS - 1), None);
        assert_eq!(ask_quiet_since(Some(100), 100 + ASK_QUIET_SECS), Some(100));
    }

    fn setup() -> (
        MemoryRepo,
        SigningKey,
        Root,
        CollectionDriver<MemoryRepoSnapshot>,
    ) {
        let key = SigningKey::from_bytes(&[0x51; 32]);
        let mut store = MemoryRepo::for_host(key.verifying_key());
        let policy = AdmissionPolicy::direct(key.verifying_key());
        let root = store
            .collection("rules", CollectionPolicy::new(policy.clone(), policy))
            .unwrap();
        store.commit(root, &key, program().unwrap()).unwrap();
        let driver =
            CollectionDriver::new(root, ENVIRONMENT_ANCHOR, key.clone(), Bounds::default());
        (store, key, root, driver)
    }

    fn held(store: &mut MemoryRepo, root: Root) -> TribleSet {
        root.read(&store.snapshot().unwrap()).unwrap()
    }

    fn tick(store: &mut MemoryRepo, key: &SigningKey, root: Root, at: i64, sensed: &Sensed) -> Id {
        let fragment = tick_fragment(quota::unix_point(at).unwrap(), sensed).unwrap();
        let id = fragment.root().unwrap();
        store.commit(root, key, fragment).unwrap();
        id
    }

    #[test]
    fn the_same_facts_give_the_same_request_twice() {
        let (mut store, key, root, mut driver) = setup();
        let goal = *genid();
        let sensed = Sensed {
            eligible: vec![goal],
            quota_ok: Some(*genid()),
            ask_open: vec![*genid()],
            ask_quiet_since: Some(0),
        };
        let first = tick(&mut store, &key, root, 1, &sensed);
        assert!(settle(&mut driver, &mut store, 8).unwrap().settled);
        let second = tick(&mut store, &key, root, 2, &sensed);
        assert!(settle(&mut driver, &mut store, 8).unwrap().settled);
        let facts = held(&mut store, root);
        let one = run_requests_in(&facts, first);
        let two = run_requests_in(&facts, second);
        assert_eq!(one.len(), 1);
        assert_eq!(one, two, "both ticks name the same request");
        assert_eq!(run_requests(&facts).len(), 1);
        assert_eq!(
            nudges_in(&facts, first).keys().collect::<Vec<_>>(),
            nudges_in(&facts, second).keys().collect::<Vec<_>>()
        );
        // A fresh driver over the same durable facts concludes nothing new.
        let mut reopened =
            CollectionDriver::new(root, ENVIRONMENT_ANCHOR, key.clone(), Bounds::default());
        let again = settle(&mut reopened, &mut store, 8).unwrap();
        assert_eq!(again.commits, 0);
        assert_eq!(held(&mut store, root), facts);
    }

    #[test]
    fn a_newly_eligible_goal_wakes_the_driver() {
        let (mut store, key, root, mut driver) = setup();
        let quota = Some(*genid());
        tick(
            &mut store,
            &key,
            root,
            1,
            &Sensed {
                quota_ok: quota,
                ..Sensed::default()
            },
        );
        let quiet = settle(&mut driver, &mut store, 8).unwrap();
        assert!(quiet.settled);
        assert!(run_requests(&held(&mut store, root)).is_empty());
        // Nothing changed: the next poll does no work.
        assert_eq!(settle(&mut driver, &mut store, 8).unwrap().polls, 1);
        let goal = *genid();
        let woke = tick(
            &mut store,
            &key,
            root,
            2,
            &Sensed {
                eligible: vec![goal],
                quota_ok: quota,
                ..Sensed::default()
            },
        );
        let step = settle(&mut driver, &mut store, 8).unwrap();
        assert!(step.firings > 0 && step.commits > 0, "{step:?}");
        let requests = run_requests_in(&held(&mut store, root), woke);
        assert_eq!(requests.values().copied().collect::<Vec<_>>(), vec![goal]);
    }

    #[test]
    fn premises_from_different_ticks_do_not_join() {
        let (mut store, key, root, mut driver) = setup();
        tick(
            &mut store,
            &key,
            root,
            1,
            &Sensed {
                quota_ok: Some(*genid()),
                ..Sensed::default()
            },
        );
        tick(
            &mut store,
            &key,
            root,
            2,
            &Sensed {
                eligible: vec![*genid()],
                ..Sensed::default()
            },
        );
        assert!(settle(&mut driver, &mut store, 8).unwrap().settled);
        assert!(run_requests(&held(&mut store, root)).is_empty());
    }
}
