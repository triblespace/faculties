//! `quota` — immutable provider-reported quota observations.
//!
//! Live data is one fixed native collection, `quota`. `record` publishes one
//! observation; `show` reads the latest per `(provider, account, window,
//! bucket)` from one frozen view and reports how many observations it examined.
//! `provider_status` reads each provider's own status page and publishes one
//! status observation per page it could read; `show` reports the latest per
//! provider beside the quota, again with how many it examined.

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

use crate::quota::status::{self, PageItem, ProviderStatus};
use crate::quota::{self, Observation, TextHandle};
use crate::schemas::quota::{provider_status, quota as attributes, DEFAULT_SCOPE_ID};
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

/// The latest observation of one `(provider, account, window, bucket)`.
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
    /// The allowance's name; `None` groups the observations without one.
    pub bucket: Option<String>,
    /// TAI nanoseconds.
    pub observed: i128,
    pub used_percent: Vec<f64>,
    /// TAI nanoseconds.
    pub resets_at: Vec<i128>,
    pub host: Vec<String>,
}

/// One published status observation and its exact publication receipt.
#[derive(Clone, Debug)]
pub struct RecordedStatus {
    pub event: Id,
    pub commit: CollectionCommit,
    pub status: ProviderStatus,
}

/// What became of one provider's status page: recorded, or why not. A
/// failure writes nothing for that provider.
#[derive(Debug)]
pub struct StatusOutcome {
    pub provider: Id,
    pub url: &'static str,
    pub result: Result<RecordedStatus>,
}

/// The latest status observation of one provider: the provider's own claim.
///
/// Fields are lists for the same reason as in [`QuotaGroup`]; components and
/// incidents are sorted and empty when the page listed none.
#[derive(Clone, Debug, PartialEq)]
pub struct ProviderStatusView {
    pub event: Id,
    pub provider: Id,
    pub provider_name: Option<String>,
    /// TAI nanoseconds.
    pub observed: i128,
    pub indicator: Vec<String>,
    pub description: Vec<String>,
    pub components: Vec<PageItem>,
    pub incidents: Vec<PageItem>,
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
    /// Distinct status observations carrying a provider and an observed time.
    pub status_examined: usize,
    /// The latest status observation per provider.
    pub statuses: Vec<ProviderStatusView>,
    /// Why the status observations could not be read, when they could not.
    /// The quota groups are still shown: a status text that has not arrived
    /// yet must not hide readings that are fine.
    pub status_error: Option<String>,
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

    /// Read every provider's status page through `fetch` and publish one
    /// observation per page that was fetched and parsed. `clock` gives unix
    /// seconds and is asked once per page, after its body arrived. A page
    /// that fails yields its error in the outcome and writes nothing; the
    /// others are still recorded. The pile is opened only after all fetches.
    pub fn provider_status(
        &self,
        mut fetch: impl FnMut(&str) -> Result<Vec<u8>>,
        mut clock: impl FnMut() -> Result<i64>,
        host: &str,
    ) -> Result<Vec<StatusOutcome>> {
        let readings: Vec<_> = status::STATUS_PAGES
            .iter()
            .map(|&(provider, url)| {
                let reading = fetch(url).and_then(|body| {
                    let status = status::parse_summary(provider, &body, clock()?, host)
                        .with_context(|| format!("read {url}"))?;
                    let (fragment, event) = status::status_fragment(&status)?;
                    Ok((status, fragment, event))
                });
                (provider, url, reading)
            })
            .collect();
        let outcome = |provider, url, result| StatusOutcome {
            provider,
            url,
            result,
        };
        // Every page failed: report them without opening the pile.
        if readings.iter().all(|(_, _, reading)| reading.is_err()) {
            return Ok(readings
                .into_iter()
                .filter_map(|(provider, url, reading)| {
                    reading
                        .err()
                        .map(|error| outcome(provider, url, Err(error)))
                })
                .collect());
        }
        self.storage.with_store(|pile, signer, runtime| {
            Ok(readings
                .into_iter()
                .map(|(provider, url, reading)| {
                    let result = reading.and_then(|(status, fragment, event)| {
                        commit_quota(&self.storage, pile, signer, fragment, runtime).map(|commit| {
                            RecordedStatus {
                                event,
                                commit,
                                status,
                            }
                        })
                    });
                    outcome(provider, url, result)
                })
                .collect())
        })
    }

    pub fn show(&self) -> Result<QuotaView> {
        self.show_at(crate::clock::tai_nanoseconds_now()?)
    }

    /// The latest observation per group, evaluated at an explicit instant.
    pub fn show_at(&self, now: i128) -> Result<QuotaView> {
        self.storage.with_store(|pile, signer, runtime| {
            observe_at(&self.storage, pile, signer, runtime, now)
        })
    }
}

/// [`Quota::show_at`] on a store the caller already holds open, so another
/// faculty's report (Orient's) can carry the same view. Run it outside any
/// async runtime: the read acquires through `runtime`.
pub fn observe_at(
    storage: &crate::storage::Storage,
    pile: &mut FacultyStore,
    signer: &SigningKey,
    runtime: &Arc<tokio::runtime::Runtime>,
    now: i128,
) -> Result<QuotaView> {
    let (facts, snapshot) = maintain_and_observe(storage, pile, signer, runtime)?;
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
            let provider_name = provider_name(&facts, &snapshot, provider)?;
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
                window_secs: i64::try_from(row.key.window / 1_000_000_000).unwrap_or(i64::MAX),
                bucket: row
                    .key
                    .bucket
                    .map(|bucket| read_text(&snapshot, bucket, "bucket"))
                    .transpose()?,
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
            &left.bucket,
        )
            .cmp(&(
                &right.provider_name,
                right.provider,
                &right.account,
                right.window_secs,
                &right.bucket,
            ))
    });
    let (status_examined, statuses, status_error) = match observe_statuses(&facts, &snapshot) {
        Ok((examined, statuses)) => (examined, statuses, None),
        Err(error) => (0, Vec::new(), Some(format!("{error:#}"))),
    };
    Ok(QuotaView {
        now,
        examined,
        groups,
        status_examined,
        statuses,
        status_error,
    })
}

/// The latest status observation per provider, read at the point of use.
fn observe_statuses(
    facts: &FactArchive,
    snapshot: &AcquiringReader<FacultySnapshot>,
) -> Result<(usize, Vec<ProviderStatusView>)> {
    let rows = status::status_rows(facts);
    let examined = rows
        .iter()
        .map(|row| row.event)
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    let texts = |handles: Vec<TextHandle>, what: &str| -> Result<Vec<String>> {
        let mut texts = handles
            .into_iter()
            .map(|handle| read_text(snapshot, handle, what))
            .collect::<Result<Vec<_>>>()?;
        texts.sort();
        Ok(texts)
    };
    let items = |pairs: Vec<(TextHandle, TextHandle)>, what: &str| -> Result<Vec<PageItem>> {
        let mut items = pairs
            .into_iter()
            .map(|(name, state)| {
                Ok(PageItem {
                    name: read_text(snapshot, name, what)?,
                    status: read_text(snapshot, state, what)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        items.sort();
        Ok(items)
    };
    let mut statuses = status::latest_status_per_provider(rows)
        .into_values()
        .map(|row| {
            let event = row.event;
            Ok(ProviderStatusView {
                event,
                provider: row.provider,
                provider_name: provider_name(facts, snapshot, row.provider)?,
                observed: row.observed,
                indicator: texts(
                    find!(
                        text: TextHandle,
                        pattern!(facts, [{ event @ provider_status::indicator: ?text }])
                    )
                    .collect(),
                    "status indicator",
                )?,
                description: texts(
                    find!(
                        text: TextHandle,
                        pattern!(facts, [{ event @ provider_status::description: ?text }])
                    )
                    .collect(),
                    "status description",
                )?,
                components: items(
                    find!(
                        (name: TextHandle, state: TextHandle),
                        pattern!(facts, [
                            { event @ provider_status::component: _?child },
                            { _?child @ metadata::name: ?name, provider_status::state: ?state }
                        ])
                    )
                    .collect(),
                    "status component",
                )?,
                incidents: items(
                    find!(
                        (name: TextHandle, state: TextHandle),
                        pattern!(facts, [
                            { event @ provider_status::incident: _?child },
                            { _?child @ metadata::name: ?name, provider_status::state: ?state }
                        ])
                    )
                    .collect(),
                    "status incident",
                )?,
                host: texts(
                    find!(
                        host: TextHandle,
                        pattern!(facts, [{ event @ attributes::host: ?host }])
                    )
                    .collect(),
                    "host",
                )?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    statuses.sort_by(|left, right| {
        (&left.provider_name, left.provider).cmp(&(&right.provider_name, right.provider))
    });
    Ok((examined, statuses))
}

/// This build's name for a provider, else the name the pile carries for it.
fn provider_name(
    facts: &FactArchive,
    snapshot: &AcquiringReader<FacultySnapshot>,
    provider: Id,
) -> Result<Option<String>> {
    match quota::known_provider_name(provider) {
        Some(name) => Ok(Some(name.to_owned())),
        None => find!(
            name: TextHandle,
            pattern!(facts, [{ provider @ metadata::name: ?name }])
        )
        .next()
        .map(|name| read_text(snapshot, name, "provider name"))
        .transpose(),
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
            bucket: None,
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

    fn codex_reading(bucket: Option<&str>, used: f64) -> Observation {
        let mut codex = reading(604_800, used, NOW - 60);
        codex.provider = PROVIDER_CODEX;
        codex.bucket = bucket.map(str::to_owned);
        codex
    }

    #[test]
    fn two_buckets_with_the_same_window_are_separate_groups() {
        let fixture = fixture();
        fixture
            .quota
            .record(codex_reading(Some("codex"), 21.0))
            .unwrap();
        fixture
            .quota
            .record(codex_reading(Some("codex_other"), 64.0))
            .unwrap();

        let view = fixture.quota.show_at(now()).unwrap();
        assert_eq!(view.examined, 2);
        let shape: Vec<(Option<&str>, i64, Vec<f64>)> = view
            .groups
            .iter()
            .map(|group| {
                (
                    group.bucket.as_deref(),
                    group.window_secs,
                    group.used_percent.clone(),
                )
            })
            .collect();
        assert_eq!(
            shape,
            vec![
                (Some("codex"), 604_800, vec![21.0]),
                (Some("codex_other"), 604_800, vec![64.0])
            ]
        );
        let text = rendered(&view);
        assert!(text.contains("codex 7d bucket codex acct "), "{text}");
        assert!(text.contains("codex 7d bucket codex_other acct "), "{text}");
    }

    #[test]
    fn no_bucket_and_a_bucket_are_separate_groups() {
        let fixture = fixture();
        fixture.quota.record(codex_reading(None, 21.0)).unwrap();
        fixture
            .quota
            .record(codex_reading(Some("codex"), 30.0))
            .unwrap();

        let view = fixture.quota.show_at(now()).unwrap();
        assert_eq!(view.examined, 2);
        let shape: Vec<(Option<&str>, Vec<f64>)> = view
            .groups
            .iter()
            .map(|group| (group.bucket.as_deref(), group.used_percent.clone()))
            .collect();
        assert_eq!(shape, vec![(None, vec![21.0]), (Some("codex"), vec![30.0])]);
        let text = rendered(&view);
        assert!(text.contains("codex 7d acct "), "{text}");
        let mut json = String::new();
        crate::quota::render::show_json(
            &view,
            &mut crate::out::Out::new(&mut |part| {
                if let crate::out::Part::Text { text: part } = part {
                    json.push_str(&part);
                }
                Ok(())
            }),
        )
        .unwrap();
        let document: serde_json::Value = serde_json::from_str(&json).unwrap();
        let buckets: Vec<&serde_json::Value> = document["groups"]
            .as_array()
            .unwrap()
            .iter()
            .map(|group| &group["bucket"])
            .collect();
        assert_eq!(
            buckets,
            vec![&serde_json::Value::Null, &serde_json::json!("codex")]
        );
    }

    #[test]
    fn an_empty_bucket_is_refused() {
        let fixture = fixture();
        assert!(fixture.quota.record(codex_reading(Some(""), 21.0)).is_err());
        assert_eq!(fixture.quota.show_at(now()).unwrap().examined, 0);
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

    const OPENAI_OUTAGE: &[u8] = include_bytes!("fixtures/openai_outage.json");
    const CLAUDE_OPERATIONAL: &[u8] = include_bytes!("fixtures/claude_operational.json");

    /// A stand-in for the network: one body per status page, or a failure.
    fn pages(
        claude: Option<&'static [u8]>,
        openai: Option<&'static [u8]>,
    ) -> impl FnMut(&str) -> Result<Vec<u8>> {
        move |url: &str| {
            let body = if url.contains("status.claude.com") {
                claude
            } else if url.contains("status.openai.com") {
                openai
            } else {
                panic!("unexpected status page {url}")
            };
            body.map(<[u8]>::to_vec)
                .ok_or_else(|| anyhow::anyhow!("connection refused (test)"))
        }
    }

    #[test]
    fn an_outage_is_recorded_and_shown_beside_the_quota() {
        let fixture = fixture();
        fixture
            .quota
            .record(reading(18_000, 3.0, NOW - 60))
            .unwrap();
        let outcomes = fixture
            .quota
            .provider_status(
                pages(Some(CLAUDE_OPERATIONAL), Some(OPENAI_OUTAGE)),
                || Ok(NOW - 90),
                "sky",
            )
            .unwrap();
        assert_eq!(outcomes.len(), 2);
        assert!(outcomes.iter().all(|outcome| outcome.result.is_ok()));

        let view = fixture.quota.show_at(now()).unwrap();
        assert_eq!(view.examined, 1);
        assert_eq!(view.status_examined, 2);
        let text = rendered(&view);
        assert!(
            text.contains("1 observations examined, 1 groups shown"),
            "{text}"
        );
        assert!(
            text.contains(
                "provider status: 2 observations examined, 2 providers shown (the latest per \
                 provider, by observed time)"
            ),
            "{text}"
        );
        let lines: Vec<&str> = text.lines().collect();
        assert!(
            lines.contains(
                &"claude status none (All Systems Operational); age 1m; host sky; provider's own \
                  status page"
            ),
            "{text}"
        );
        assert!(
            lines.contains(
                &"codex status critical (Major System Outage); not operational: ChatGPT \
                  full_outage, Codex full_outage; incidents: \"Issues with Codex\" identified; \
                  age 1m; host sky; provider's own status page"
            ),
            "{text}"
        );
    }

    fn receipt(outcomes: &[StatusOutcome]) -> String {
        let mut text = String::new();
        crate::quota::render::provider_status(
            outcomes,
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
    fn a_failed_page_records_nothing_for_that_provider() {
        let fixture = fixture();
        let outcomes = fixture
            .quota
            .provider_status(
                pages(Some(CLAUDE_OPERATIONAL), None),
                || Ok(NOW - 30),
                "sky",
            )
            .unwrap();
        let shape: Vec<(Id, bool)> = outcomes
            .iter()
            .map(|outcome| (outcome.provider, outcome.result.is_ok()))
            .collect();
        assert_eq!(
            shape,
            vec![(PROVIDER_CLAUDE, true), (PROVIDER_CODEX, false)]
        );
        let text = receipt(&outcomes);
        assert!(
            text.contains("FAILED codex: connection refused (test); nothing recorded for codex"),
            "{text}"
        );
        assert!(
            text.contains(": claude status none (All Systems Operational)"),
            "{text}"
        );

        let view = fixture.quota.show_at(now()).unwrap();
        assert_eq!(view.status_examined, 1);
        let providers: Vec<Id> = view.statuses.iter().map(|status| status.provider).collect();
        assert_eq!(providers, vec![PROVIDER_CLAUDE]);
        let text = rendered(&view);
        assert!(!text.contains("codex status"), "{text}");
        assert!(
            text.contains("0 observations examined, 0 groups shown"),
            "{text}"
        );
        assert!(text.contains("- (no observations recorded)"), "{text}");
    }

    #[test]
    fn an_unparseable_page_records_nothing_and_the_other_still_records() {
        let fixture = fixture();
        let outcomes = fixture
            .quota
            .provider_status(
                pages(
                    Some(&b"<html>we'll be right back</html>"[..]),
                    Some(OPENAI_OUTAGE),
                ),
                || Ok(NOW - 30),
                "sky",
            )
            .unwrap();
        assert!(outcomes[0].result.is_err());
        assert!(outcomes[1].result.is_ok());
        let view = fixture.quota.show_at(now()).unwrap();
        assert_eq!(view.status_examined, 1);
        assert_eq!(view.statuses.len(), 1);
        assert_eq!(view.statuses[0].provider, PROVIDER_CODEX);
        assert_eq!(view.statuses[0].indicator, vec!["critical".to_owned()]);
    }

    #[test]
    fn when_every_page_fails_nothing_is_written_and_show_is_unchanged() {
        let fixture = fixture();
        let outcomes = fixture
            .quota
            .provider_status(pages(None, None), || Ok(NOW - 30), "sky")
            .unwrap();
        assert!(outcomes.iter().all(|outcome| outcome.result.is_err()));
        let view = fixture.quota.show_at(now()).unwrap();
        assert_eq!(view.status_examined, 0);
        assert!(!rendered(&view).contains("provider status"));
    }

    #[test]
    fn a_status_older_than_an_hour_is_labelled_stale_and_json_names_its_source() {
        let fixture = fixture();
        fixture
            .quota
            .provider_status(
                pages(Some(CLAUDE_OPERATIONAL), Some(OPENAI_OUTAGE)),
                || Ok(NOW - 2 * HOUR),
                "sky",
            )
            .unwrap();
        let view = fixture.quota.show_at(now()).unwrap();
        let text = rendered(&view);
        let claude = text
            .lines()
            .find(|line| line.starts_with("claude status"))
            .unwrap();
        assert!(claude.contains("age 2h00m STALE"), "{text}");
        let mut json = String::new();
        crate::quota::render::show_json(
            &view,
            &mut crate::out::Out::new(&mut |part| {
                if let crate::out::Part::Text { text: part } = part {
                    json.push_str(&part);
                }
                Ok(())
            }),
        )
        .unwrap();
        let document: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(document["status_examined"], 2);
        let codex = &document["provider_status"][1];
        assert_eq!(codex["provider"], "codex");
        assert_eq!(codex["source"], "provider's own status page");
        assert_eq!(codex["indicator"], "critical");
        assert_eq!(codex["stale"], true);
        assert_eq!(
            codex["incidents"],
            serde_json::json!([{ "name": "Issues with Codex", "status": "identified" }])
        );
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
