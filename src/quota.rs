//! Provider-reported subscription quota, one immutable observation at a time.
//!
//! An observation is the intrinsic identity of its exact facts, so recording
//! the same reading twice converges on one event. That is idempotence and
//! nothing more: nothing here computes an observation id in order to find one.
//! The current reading per `(provider, account, window)` is a pure maximum
//! over `(observed time, event id)`, which is permutation-independent and
//! commutes with collection union.
//!
//! Reads are typed queries at the point of use. A fact this build does not
//! model is ignored, and a record missing a field it does not need for
//! grouping still shows, with that field reported as missing.

pub mod cli;
pub mod mcp;
pub mod operations;
pub mod render;

pub use operations::{Quota, QuotaGroup, QuotaView, RecordedObservation};

use std::collections::BTreeMap;

use anyhow::{bail, Result};
use hifitime::{Duration, Epoch};
use triblespace::core::inline::encodings::time::Lower;
use triblespace::core::metadata;
use triblespace::core::query::TriblePattern;
use triblespace::prelude::*;

use crate::schemas::quota::{quota, KIND_QUOTA_OBSERVATION, PROVIDERS};

pub type TextHandle = Inline<inlineencodings::Handle<blobencodings::UTF8String>>;
pub type IntervalValue = Inline<inlineencodings::NsTAIInterval>;

const NANOS_PER_SECOND: i128 = 1_000_000_000;

/// Pace margin in percentage points: within it a window is "on pace".
pub const PACE_MARGIN_POINTS: f64 = 5.0;

/// An observation older than this is labelled stale. It is still shown.
pub const STALE_AFTER_SECONDS: i64 = 6 * 3_600;

/// Longest account digest accepted for writing. A SHA-256 hex is 64.
pub const MAX_ACCOUNT_LEN: usize = 128;

/// One reading as a caller supplies it, before anything is stored.
#[derive(Clone, Debug, PartialEq)]
pub struct Observation {
    pub provider: Id,
    pub account: String,
    pub window_secs: u64,
    pub used_percent: f64,
    /// Unix seconds.
    pub resets_at: i64,
    /// Unix seconds.
    pub observed_at: i64,
    pub host: String,
}

/// The provider id this build writes under `name`, case-insensitively.
pub fn provider_id(name: &str) -> Result<Id> {
    let wanted = name.trim().to_lowercase();
    match PROVIDERS.iter().find(|(_, known)| *known == wanted) {
        Some((id, _)) => Ok(*id),
        None => bail!(
            "unknown provider {name:?}; this build records {}. A new provider needs a minted id \
             in faculties::schemas::quota::PROVIDERS",
            PROVIDERS
                .iter()
                .map(|(_, known)| *known)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// This build's name for a provider id, if it knows one.
pub fn known_provider_name(provider: Id) -> Option<&'static str> {
    PROVIDERS
        .iter()
        .find(|(id, _)| *id == provider)
        .map(|(_, name)| *name)
}

/// Writer-side checks. Readers never apply these: they show what is there.
pub fn validate(observation: &Observation) -> Result<()> {
    if known_provider_name(observation.provider).is_none() {
        bail!(
            "provider {:x} is not one this build records",
            observation.provider
        );
    }
    let account = observation.account.as_str();
    if account.is_empty() {
        bail!("account digest is empty");
    }
    if account.len() > MAX_ACCOUNT_LEN {
        bail!(
            "account digest is longer than {MAX_ACCOUNT_LEN} bytes; pass a digest, not the account"
        );
    }
    if account.contains('@') || account.chars().any(char::is_whitespace) {
        bail!(
            "account must be an opaque digest (no '@', no whitespace); never pass a raw account \
             id or email"
        );
    }
    if observation.window_secs == 0 {
        bail!("window length must be positive");
    }
    if !observation.used_percent.is_finite() || observation.used_percent < 0.0 {
        bail!(
            "used percentage must be a finite, non-negative number, got {}",
            observation.used_percent
        );
    }
    if observation.host.trim().is_empty() {
        bail!("host is empty");
    }
    Ok(())
}

/// Encode unix seconds as a point-valued interval.
pub fn unix_point(seconds: i64) -> Result<IntervalValue> {
    crate::clock::point(Epoch::from_unix_seconds(seconds as f64))
}

/// TAI nanoseconds (the encoding's coordinate) for unix seconds.
pub fn unix_to_tai_ns(seconds: i64) -> i128 {
    Epoch::from_unix_seconds(seconds as f64)
        .to_tai_duration()
        .total_nanoseconds()
}

/// Unix seconds, rounded down, for a TAI nanosecond coordinate.
pub fn tai_ns_to_unix(nanos: i128) -> i64 {
    Epoch::from_tai_duration(Duration::from_total_nanoseconds(nanos))
        .to_unix_seconds()
        .floor() as i64
}

/// Build one observation and the provider name it is recorded under.
///
/// Returns the fragment to commit and the observation's intrinsic id. The
/// provider entity's `metadata::name` travels with every record so a reader
/// that does not know the id can still name it; it is the same fact each
/// time and never part of the observation's identity.
pub fn observation_fragment(observation: &Observation) -> Result<(Fragment, Id)> {
    validate(observation)?;
    let provider = observation.provider;
    let window = i128::from(observation.window_secs) * NANOS_PER_SECOND;
    let resets_at = unix_point(observation.resets_at)?;
    let observed_at = unix_point(observation.observed_at)?;

    let mut fragment = Fragment::empty();
    let account: TextHandle = fragment.put(observation.account.clone());
    let host: TextHandle = fragment.put(observation.host.trim().to_owned());
    let record = entity! {
        metadata::tag: &KIND_QUOTA_OBSERVATION,
        quota::provider: provider,
        quota::account: account,
        quota::window: window,
        quota::used_percent: observation.used_percent,
        quota::resets_at: resets_at,
        metadata::created_at: observed_at,
        quota::host: host,
    };
    let event = record
        .root()
        .expect("a Quota observation has one intrinsic root");
    fragment += record;

    let name = known_provider_name(provider).expect("validated provider");
    let name: TextHandle = fragment.put(name.to_owned());
    fragment += entity! { ExclusiveId::force_ref(&provider) @ metadata::name: name };
    Ok((fragment, event))
}

/// The fields an observation must carry to be grouped and ordered.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct GroupKey {
    pub provider: Id,
    pub account: TextHandle,
    /// Window length in nanoseconds.
    pub window: i128,
}

/// One grouped observation: its key, its id, and when it was observed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ObservationRow {
    pub event: Id,
    pub key: GroupKey,
    /// TAI nanoseconds (the interval's lower bound).
    pub observed: i128,
}

/// Every observation that carries the grouping fields, through one typed
/// query. Anything else in the collection is not this read type.
pub fn observation_rows<P>(facts: &P) -> Vec<ObservationRow>
where
    P: TriblePattern + ?Sized,
{
    find!(
        (event: Id, provider: Id, account: TextHandle, window: i128, observed: Lower),
        pattern!(facts, [{ ?event @
            metadata::tag: &KIND_QUOTA_OBSERVATION,
            quota::provider: ?provider,
            quota::account: ?account,
            quota::window: ?window,
            metadata::created_at: ?observed,
        }])
    )
    .map(
        |(event, provider, account, window, observed)| ObservationRow {
            event,
            key: GroupKey {
                provider,
                account,
                window,
            },
            observed: observed.0,
        },
    )
    .collect()
}

/// The latest observation per group: larger observed time wins, the larger
/// event id breaks a tie. A pure maximum in a total order.
pub fn latest_per_group(
    rows: impl IntoIterator<Item = ObservationRow>,
) -> BTreeMap<GroupKey, ObservationRow> {
    let mut latest = BTreeMap::<GroupKey, ObservationRow>::new();
    for row in rows {
        let replace = latest
            .get(&row.key)
            .is_none_or(|current| (row.observed, row.event) > (current.observed, current.event));
        if replace {
            latest.insert(row.key, row);
        }
    }
    latest
}

/// Share of the window that has elapsed at `now`, in percent, clamped to
/// `[0, 100]`. The window is taken to start `window` before `resets_at`.
pub fn elapsed_percent(now: i128, resets_at: i128, window: i128) -> f64 {
    if window <= 0 {
        return 100.0;
    }
    let start = resets_at - window;
    let elapsed = (now - start) as f64 / window as f64 * 100.0;
    elapsed.clamp(0.0, 100.0)
}

/// A planning heuristic: how used compares with elapsed for one window.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Pace {
    /// Used is more than the margin below elapsed: room to spend faster.
    Under,
    /// Used is within the margin of elapsed.
    On,
    /// Used is more than the margin above elapsed: the window runs out early
    /// at this rate.
    Over,
    /// The reset time has passed since the reading, so the reading describes
    /// a window that is over; no verdict about the current one.
    Reset,
}

impl Pace {
    pub fn label(self) -> &'static str {
        match self {
            Pace::Under => "under pace",
            Pace::On => "on pace",
            Pace::Over => "over pace",
            Pace::Reset => "reset since observed",
        }
    }
}

/// Verdict for one reading at `now`.
pub fn pace(now: i128, used: f64, resets_at: i128, window: i128) -> Pace {
    if now >= resets_at {
        return Pace::Reset;
    }
    let elapsed = elapsed_percent(now, resets_at, window);
    if used < elapsed - PACE_MARGIN_POINTS {
        Pace::Under
    } else if used > elapsed + PACE_MARGIN_POINTS {
        Pace::Over
    } else {
        Pace::On
    }
}

/// The local short host name: the first label of `gethostname(2)`.
pub fn local_host() -> Result<String> {
    let mut buffer = [0_u8; 256];
    // SAFETY: the pointer and length describe one live, writable buffer.
    let rc = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) };
    if rc != 0 {
        bail!("gethostname failed: {}", std::io::Error::last_os_error());
    }
    let end = buffer
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(buffer.len());
    let name = std::str::from_utf8(&buffer[..end])
        .map_err(|_| anyhow::anyhow!("host name is not UTF-8; pass --host"))?;
    let short = name.split('.').next().unwrap_or(name).trim();
    if short.is_empty() {
        bail!("host name is empty; pass --host");
    }
    Ok(short.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schemas::quota::{PROVIDER_CLAUDE, PROVIDER_CODEX};

    fn observation() -> Observation {
        Observation {
            provider: PROVIDER_CLAUDE,
            account: "a1b2c3".to_owned(),
            window_secs: 18_000,
            used_percent: 3.0,
            resets_at: 1_792_894_800,
            observed_at: 1_792_886_255,
            host: "sky".to_owned(),
        }
    }

    #[test]
    fn published_quota_ids_remain_byte_exact() {
        use crate::schemas::quota::{DEFAULT_SCOPE_ID, KIND_QUOTA_OBSERVATION};
        assert_eq!(
            DEFAULT_SCOPE_ID,
            Id::from_hex("6B6C08A47B334284A56279D961255250").unwrap()
        );
        assert_eq!(
            KIND_QUOTA_OBSERVATION,
            Id::from_hex("92B1A5113408368BF20912C6EBFAC6C1").unwrap()
        );
        assert_eq!(
            PROVIDER_CLAUDE,
            Id::from_hex("99B579DE6F864BDD2007148BCCDAC2A0").unwrap()
        );
        assert_eq!(
            PROVIDER_CODEX,
            Id::from_hex("A7FA0091FA5E1A63BB8F0449DB8676C0").unwrap()
        );
    }

    #[test]
    fn identical_observations_build_identical_fragments() {
        let (first, first_id) = observation_fragment(&observation()).unwrap();
        let (second, second_id) = observation_fragment(&observation()).unwrap();
        assert_eq!(first, second);
        assert_eq!(first_id, second_id);
        let mut other = observation();
        other.used_percent = 4.0;
        assert_ne!(observation_fragment(&other).unwrap().1, first_id);
    }

    #[test]
    fn fragment_reads_back_through_the_typed_queries() {
        let (fragment, event) = observation_fragment(&observation()).unwrap();
        let rows = observation_rows(fragment.facts());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].event, event);
        assert_eq!(rows[0].key.provider, PROVIDER_CLAUDE);
        assert_eq!(rows[0].key.window, 18_000 * NANOS_PER_SECOND);
        assert_eq!(tai_ns_to_unix(rows[0].observed), 1_792_886_255);
        let facts = fragment.facts();
        let used: Vec<f64> = find!(
            used: f64,
            pattern!(facts, [{ event @ quota::used_percent: ?used }])
        )
        .collect();
        assert_eq!(used, vec![3.0]);
        let resets: Vec<Lower> = find!(
            at: Lower,
            pattern!(facts, [{ event @ quota::resets_at: ?at }])
        )
        .collect();
        assert_eq!(resets.len(), 1);
        assert_eq!(tai_ns_to_unix(resets[0].0), 1_792_894_800);
        let named = exists!(pattern!(facts, [{ PROVIDER_CLAUDE @ metadata::name: _?name }]));
        assert!(named);
    }

    #[test]
    fn a_record_without_optional_fields_still_groups() {
        let account: TextHandle = Inline::new([7; 32]);
        let observed = unix_point(100).unwrap();
        let bare = entity! {
            metadata::tag: &KIND_QUOTA_OBSERVATION,
            quota::provider: PROVIDER_CODEX,
            quota::account: account,
            quota::window: 60_i128 * NANOS_PER_SECOND,
            metadata::created_at: observed,
        };
        let event = bare.root().unwrap();
        let rows = observation_rows(bare.facts());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].event, event);
    }

    #[test]
    fn writer_refuses_what_is_not_a_reading() {
        let mut email = observation();
        email.account = "someone@example.org".to_owned();
        assert!(observation_fragment(&email).is_err());
        let mut spaced = observation();
        spaced.account = "two words".to_owned();
        assert!(observation_fragment(&spaced).is_err());
        let mut nan = observation();
        nan.used_percent = f64::NAN;
        assert!(observation_fragment(&nan).is_err());
        let mut negative = observation();
        negative.used_percent = -1.0;
        assert!(observation_fragment(&negative).is_err());
        let mut zero = observation();
        zero.window_secs = 0;
        assert!(observation_fragment(&zero).is_err());
        let mut over = observation();
        over.used_percent = 104.5;
        assert!(observation_fragment(&over).is_ok());
        assert!(provider_id("Claude").is_ok());
        assert!(provider_id("gemini").is_err());
    }

    #[test]
    fn latest_is_permutation_independent_and_ties_break_on_event_id() {
        let key = GroupKey {
            provider: PROVIDER_CLAUDE,
            account: Inline::new([1; 32]),
            window: 10,
        };
        let row = |event: u8, observed: i128| ObservationRow {
            event: Id::new([event; 16]).unwrap(),
            key,
            observed,
        };
        let (low, high, later) = (row(2, 50), row(3, 50), row(1, 51));
        for rows in [vec![low, high], vec![high, low]] {
            assert_eq!(latest_per_group(rows)[&key], high);
        }
        for rows in [vec![low, high, later], vec![later, high, low]] {
            assert_eq!(latest_per_group(rows)[&key], later);
        }
    }

    #[test]
    fn elapsed_and_pace_follow_the_stated_rule() {
        let window = 100 * NANOS_PER_SECOND;
        let resets = 1_000 * NANOS_PER_SECOND;
        let at = |seconds: i128| seconds * NANOS_PER_SECOND;
        assert_eq!(elapsed_percent(at(900), resets, window), 0.0);
        assert_eq!(elapsed_percent(at(950), resets, window), 50.0);
        assert_eq!(elapsed_percent(at(800), resets, window), 0.0);
        assert_eq!(elapsed_percent(at(1_200), resets, window), 100.0);
        assert_eq!(pace(at(950), 44.0, resets, window), Pace::Under);
        assert_eq!(pace(at(950), 45.0, resets, window), Pace::On);
        assert_eq!(pace(at(950), 55.0, resets, window), Pace::On);
        assert_eq!(pace(at(950), 56.0, resets, window), Pace::Over);
        assert_eq!(pace(at(1_000), 10.0, resets, window), Pace::Reset);
    }
}
