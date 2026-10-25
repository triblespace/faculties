//! `quota` — immutable provider-reported quota observations.
//!
//! Live data is one fixed native collection, `quota`. `record` publishes one
//! observation; `show` reads the latest per `(provider, account, window)`
//! from one frozen view and reports how many observations it examined.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use ed25519_dalek::SigningKey;
use triblespace::core::blob::encodings::succinctarchive::{
    Rank9AcceleratedSuccinctArchiveBlob, SuccinctArchiveBlob,
};
use triblespace::core::collection::{CollectionCommit, CollectionStoreExt};
use triblespace::core::inline::encodings::time::Lower;
use triblespace::core::metadata;
use triblespace::core::repo::{BlobStoreGet, SnapshotSource};
use triblespace::prelude::*;

use crate::quota::{self, Observation, TextHandle};
use crate::schemas::quota::{quota as attributes, DEFAULT_SCOPE_ID};
use crate::storage::{AcquiringReader, FactArchive, FacultySnapshot, FacultyStore};

/// Configured Quota operations. Inputs are literal; nothing ambient is read
/// here except the clock.
#[derive(Clone, Debug)]
pub struct Quota {
    storage: crate::storage::Storage,
}

/// One published observation and its exact publication receipt.
#[derive(Clone, Debug)]
pub struct RecordedObservation {
    pub event: Id,
    pub commit: CollectionCommit,
    pub observation: Observation,
}

/// The latest observation of one `(provider, account, window)`.
///
/// Fields other than the grouping key are lists because the schema does not
/// constrain cardinality: normally each holds one value, an empty list means
/// the record does not carry it, and more than one is shown as it is.
#[derive(Clone, Debug, PartialEq)]
pub struct QuotaGroup {
    pub event: Id,
    pub provider: Id,
    pub provider_name: Option<String>,
    pub account: String,
    pub window_secs: i64,
    /// TAI nanoseconds.
    pub observed: i128,
    pub used_percent: Vec<f64>,
    /// TAI nanoseconds.
    pub resets_at: Vec<i128>,
    pub host: Vec<String>,
}

/// Everything `show` reports, with the instant it was computed for.
#[derive(Clone, Debug, PartialEq)]
pub struct QuotaView {
    /// TAI nanoseconds.
    pub now: i128,
    /// Distinct observations carrying the grouping fields.
    pub examined: usize,
    pub groups: Vec<QuotaGroup>,
}

impl Quota {
    pub fn new(pile: PathBuf, key: Option<PathBuf>) -> Self {
        Self::with_storage(crate::storage::Storage::new(pile, key))
    }

    pub fn with_storage(storage: crate::storage::Storage) -> Self {
        Self { storage }
    }

    /// Publish one observation. Recording the same reading again converges
    /// on the same event and commit.
    pub fn record(&self, observation: Observation) -> Result<RecordedObservation> {
        let (fragment, event) = quota::observation_fragment(&observation)?;
        self.storage.with_store(|pile, signer, runtime| {
            let commit = commit_quota(&self.storage, pile, signer, fragment, runtime)?;
            Ok(RecordedObservation {
                event,
                commit,
                observation,
            })
        })
    }

    pub fn show(&self) -> Result<QuotaView> {
        self.show_at(crate::clock::tai_nanoseconds_now()?)
    }

    /// The latest observation per group, evaluated at an explicit instant.
    pub fn show_at(&self, now: i128) -> Result<QuotaView> {
        self.storage.with_store(|pile, signer, runtime| {
            let (facts, snapshot) = maintain_and_observe(&self.storage, pile, signer, runtime)?;
            let rows = quota::observation_rows(&facts);
            let examined = rows
                .iter()
                .map(|row| row.event)
                .collect::<std::collections::BTreeSet<_>>()
                .len();
            let mut groups = quota::latest_per_group(rows)
                .into_values()
                .map(|row| {
                    let event = row.event;
                    let provider = row.key.provider;
                    let provider_name = match quota::known_provider_name(provider) {
                        Some(name) => Some(name.to_owned()),
                        None => find!(
                            name: TextHandle,
                            pattern!(&facts, [{ provider @ metadata::name: ?name }])
                        )
                        .next()
                        .map(|name| read_text(&snapshot, name, "provider name"))
                        .transpose()?,
                    };
                    let used_percent = find!(
                        used: f64,
                        pattern!(&facts, [{ event @ attributes::used_percent: ?used }])
                    )
                    .collect();
                    let mut resets_at: Vec<i128> = find!(
                        at: Lower,
                        pattern!(&facts, [{ event @ attributes::resets_at: ?at }])
                    )
                    .map(|at| at.0)
                    .collect();
                    resets_at.sort_unstable();
                    let mut host = find!(
                        host: TextHandle,
                        pattern!(&facts, [{ event @ attributes::host: ?host }])
                    )
                    .map(|host| read_text(&snapshot, host, "host"))
                    .collect::<Result<Vec<_>>>()?;
                    host.sort();
                    Ok(QuotaGroup {
                        event,
                        provider,
                        provider_name,
                        account: read_text(&snapshot, row.key.account, "account")?,
                        window_secs: i64::try_from(row.key.window / 1_000_000_000)
                            .unwrap_or(i64::MAX),
                        observed: row.observed,
                        used_percent,
                        resets_at,
                        host,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            groups.sort_by(|left, right| {
                (
                    &left.provider_name,
                    left.provider,
                    &left.account,
                    left.window_secs,
                )
                    .cmp(&(
                        &right.provider_name,
                        right.provider,
                        &right.account,
                        right.window_secs,
                    ))
            });
            Ok(QuotaView {
                now,
                examined,
                groups,
            })
        })
    }
}

fn read_text<Store>(reader: &Store, handle: TextHandle, what: &str) -> Result<String>
where
    Store: BlobStoreGet + ?Sized,
{
    let text: anybytes::View<str> = reader
        .get(handle)
        .with_context(|| format!("read Quota {what} {}", hex::encode(handle.raw)))?;
    Ok(text.to_string())
}

/// Advance this key's own commits into the Rank9 view and freeze it.
fn maintain_and_observe(
    storage: &crate::storage::Storage,
    pile: &mut FacultyStore,
    signer: &SigningKey,
    runtime: &Arc<tokio::runtime::Runtime>,
) -> Result<(FactArchive, AcquiringReader<FacultySnapshot>)> {
    let source =
        storage.open_collection_read(pile, DEFAULT_SCOPE_ID, signer.verifying_key(), runtime)?;
    let succinct = pile.attach::<SuccinctArchiveBlob>(source, ())?;
    let rank9 = pile.attach::<Rank9AcceleratedSuccinctArchiveBlob>(source, succinct)?;
    {
        // Release the backend guard before any reader acquires through it.
        let mut local = pile.store();
        pollster::block_on(async {
            crate::storage::tolerate_own_lag(local.maintain_attached(succinct, signer).await)?;
            crate::storage::tolerate_own_lag(local.maintain_attached(rank9, signer).await)
        })
        .context("maintain Quota fact collection")?;
    }
    let snapshot = pile
        .snapshot()
        .context("freeze maintained Quota snapshot")?;
    let snapshot = AcquiringReader::new(snapshot, Arc::clone(runtime));
    let facts =
        crate::storage::acquire_facts(&snapshot, rank9).context("read Quota Rank9 collection")?;
    Ok((facts, snapshot))
}

fn commit_quota(
    storage: &crate::storage::Storage,
    pile: &mut FacultyStore,
    signer: &SigningKey,
    fragment: Fragment,
    runtime: &Arc<tokio::runtime::Runtime>,
) -> Result<CollectionCommit> {
    let collection =
        storage.open_collection_write(pile, DEFAULT_SCOPE_ID, signer.verifying_key(), runtime)?;
    crate::collection_names::require_command_write_admission_acquiring(
        pile,
        collection,
        signer,
        "Quota",
        "quota show",
        runtime,
    )?;
    let commit = pile
        .commit(collection, signer, fragment)
        .context("commit Quota observation")?;
    {
        let mut local = pile.store();
        drop(
            pollster::block_on(crate::storage::ensure_downstream(
                &mut *local,
                collection,
                signer,
            ))
            .context("Quota facts were committed, but ensuring their derived views failed")?,
        );
    }
    Ok(commit)
}

#[cfg(test)]
mod tests {
    use std::fs::{self, File};

    use super::*;
    use crate::schemas::quota::{PROVIDER_CLAUDE, PROVIDER_CODEX};
    use crate::storage::initialize_signer;

    const HOUR: i64 = 3_600;
    const NOW: i64 = 1_792_886_255;

    struct Fixture {
        _directory: tempfile::TempDir,
        pile: PathBuf,
        quota: Quota,
    }

    fn fixture() -> Fixture {
        let directory = tempfile::tempdir().unwrap();
        let pile = directory.path().join("quota.pile");
        let key = directory.path().join("quota.key");
        File::create(&pile).unwrap();
        initialize_signer(&pile, Some(&key)).unwrap();
        Fixture {
            _directory: directory,
            quota: Quota::with_storage(crate::storage::Storage::new(pile.clone(), Some(key))),
            pile,
        }
    }

    fn reading(window_secs: u64, used: f64, observed_at: i64) -> Observation {
        Observation {
            provider: PROVIDER_CLAUDE,
            account: "a1b2c3d4".to_owned(),
            window_secs,
            used_percent: used,
            resets_at: NOW + 2 * HOUR,
            observed_at,
            host: "sky".to_owned(),
        }
    }

    fn now() -> i128 {
        quota::unix_to_tai_ns(NOW)
    }

    fn rendered(view: &QuotaView) -> String {
        let mut text = String::new();
        crate::quota::render::show(
            view,
            &mut crate::out::Out::new(&mut |part| {
                if let crate::out::Part::Text { text: part } = part {
                    text.push_str(&part);
                }
                Ok(())
            }),
        )
        .unwrap();
        text
    }

    #[test]
    fn record_then_show_returns_the_latest_per_group() {
        let fixture = fixture();
        fixture
            .quota
            .record(reading(18_000, 3.0, NOW - 600))
            .unwrap();
        let later = fixture
            .quota
            .record(reading(18_000, 7.5, NOW - 60))
            .unwrap();
        fixture
            .quota
            .record(reading(18_000, 5.0, NOW - 300))
            .unwrap();

        let view = fixture.quota.show_at(now()).unwrap();
        assert_eq!(view.examined, 3);
        assert_eq!(view.groups.len(), 1);
        let group = &view.groups[0];
        assert_eq!(group.event, later.event);
        assert_eq!(group.used_percent, vec![7.5]);
        assert_eq!(group.provider_name.as_deref(), Some("claude"));
        assert_eq!(group.account, "a1b2c3d4");
        assert_eq!(group.window_secs, 18_000);
        assert_eq!(group.host, vec!["sky".to_owned()]);
        assert_eq!(quota::tai_ns_to_unix(group.observed), NOW - 60);
        assert_eq!(group.resets_at.len(), 1);
        assert_eq!(quota::tai_ns_to_unix(group.resets_at[0]), NOW + 2 * HOUR);
    }

    #[test]
    fn two_windows_and_two_providers_are_separate_groups() {
        let fixture = fixture();
        fixture
            .quota
            .record(reading(18_000, 3.0, NOW - 60))
            .unwrap();
        fixture
            .quota
            .record(reading(604_800, 8.0, NOW - 60))
            .unwrap();
        let mut codex = reading(604_800, 21.0, NOW - 60);
        codex.provider = PROVIDER_CODEX;
        fixture.quota.record(codex).unwrap();

        let view = fixture.quota.show_at(now()).unwrap();
        assert_eq!(view.examined, 3);
        let shape: Vec<(Option<&str>, i64)> = view
            .groups
            .iter()
            .map(|group| (group.provider_name.as_deref(), group.window_secs))
            .collect();
        assert_eq!(
            shape,
            vec![
                (Some("claude"), 18_000),
                (Some("claude"), 604_800),
                (Some("codex"), 604_800)
            ]
        );
    }

    #[test]
    fn an_identical_second_record_is_one_event_and_does_not_grow_the_pile() {
        let fixture = fixture();
        let first = fixture
            .quota
            .record(reading(18_000, 3.0, NOW - 60))
            .unwrap();
        let length = fs::metadata(&fixture.pile).unwrap().len();
        let second = fixture
            .quota
            .record(reading(18_000, 3.0, NOW - 60))
            .unwrap();
        assert_eq!(first.event, second.event);
        assert_eq!(first.commit, second.commit);
        assert_eq!(fs::metadata(&fixture.pile).unwrap().len(), length);

        let view = fixture.quota.show_at(now()).unwrap();
        assert_eq!(view.examined, 1);
        assert_eq!(view.groups.len(), 1);
    }

    #[test]
    fn a_stale_observation_is_shown_and_labelled_stale() {
        let fixture = fixture();
        let mut old = reading(604_800, 40.0, NOW - 7 * HOUR);
        old.resets_at = NOW + 3 * 24 * HOUR;
        fixture.quota.record(old).unwrap();
        fixture
            .quota
            .record(reading(18_000, 3.0, NOW - 60))
            .unwrap();

        let view = fixture.quota.show_at(now()).unwrap();
        assert_eq!(view.groups.len(), 2);
        let text = rendered(&view);
        assert!(
            text.contains("2 observations examined, 2 groups shown"),
            "{text}"
        );
        let weekly = text.lines().find(|line| line.contains(" 7d ")).unwrap();
        assert!(weekly.contains("used 40%"), "{text}");
        assert!(weekly.contains("STALE"), "{text}");
        let five_hour = text.lines().find(|line| line.contains(" 5h ")).unwrap();
        assert!(!five_hour.contains("STALE"), "{text}");
    }

    #[test]
    fn an_empty_collection_says_so() {
        let fixture = fixture();
        let view = fixture.quota.show_at(now()).unwrap();
        assert_eq!(view.examined, 0);
        assert!(view.groups.is_empty());
        assert!(rendered(&view).contains("0 observations examined, 0 groups shown"));
    }
}
