//! A bounded projection of recorded readings, never an inferred usage ledger.

use anyhow::Result;
use serde_json::json;
use triblespace::core::inline::encodings::time::Lower;
use triblespace::core::metadata;
use triblespace::core::query::rangeconstraint::value_range;
use triblespace::prelude::*;

use super::operations::{provider_name, read_text};
use super::{quota, QuotaGroup, TextHandle};
use crate::out::Out;
use crate::schemas::quota::KIND_QUOTA_OBSERVATION;
use crate::storage::{AcquiringReader, FactArchive, FacultySnapshot};

/// All typed matching rows from one frozen view. Repeated attributes remain
/// lists; repeated grouping fields produce separate rows with the same event.
#[derive(Clone, Debug, PartialEq)]
pub struct HistoryView {
    pub since: i64,
    pub until: i64,
    pub observations: usize,
    pub rows: Vec<QuotaGroup>,
}

pub(super) fn observe(
    facts: &FactArchive,
    snapshot: &AcquiringReader<FacultySnapshot>,
    since: i64,
    until: i64,
) -> Result<HistoryView> {
    // `value_range` has inclusive raw-byte bounds (core/query/rangeconstraint.rs).
    // NsTAIInterval orders by lower then upper (core/inline/encodings/time.rs).
    // Start with the native encoder, then make a range sentinel just below
    // `until`, including every upper half. A point-valued max would wrongly
    // exclude nonpoint intervals with the same selected lower bound.
    let min = super::unix_point(since)?;
    let mut max = super::unix_point(until)?;
    let encoded_lower = u128::from_be_bytes(max.raw[..16].try_into()?);
    max.raw[..16].copy_from_slice(&(encoded_lower - 1).to_be_bytes());
    max.raw[16..].fill(255);
    let mut rows = Vec::new();
    for (event, provider, account, window, observed) in find!(
        (event: Id, provider: Id, account: TextHandle, window: i128, observed: Lower),
        and!(
            pattern!(facts, [{ ?event @
                metadata::tag: &KIND_QUOTA_OBSERVATION,
                quota::provider: ?provider,
                quota::account: ?account,
                quota::window: ?window,
                metadata::created_at: ?observed,
            }]),
            value_range(observed, min, max),
        )
    ) {
        let used_percent = find!(
            used: f64,
            pattern!(facts, [{ event @ quota::used_percent: ?used }])
        )
        .collect::<Vec<_>>();
        let mut resets_at = find!(
            at: Lower,
            pattern!(facts, [{ event @ quota::resets_at: ?at }])
        )
        .map(|at| at.0)
        .collect::<Vec<_>>();
        resets_at.sort_unstable();
        let mut host = find!(
            host: TextHandle,
            pattern!(facts, [{ event @ quota::host: ?host }])
        )
        .map(|host| read_text(snapshot, host, "host"))
        .collect::<Result<Vec<_>>>()?;
        host.sort();
        let mut buckets = find!(
            bucket: TextHandle,
            pattern!(facts, [{ event @ quota::bucket: ?bucket }])
        )
        .map(|bucket| read_text(snapshot, bucket, "bucket").map(Some))
        .collect::<Result<Vec<_>>>()?;
        if buckets.is_empty() {
            buckets.push(None);
        }
        let account = read_text(snapshot, account, "account")?;
        let provider_name = provider_name(facts, snapshot, provider)?;
        for bucket in buckets {
            rows.push(QuotaGroup {
                event,
                provider,
                provider_name: provider_name.clone(),
                account: account.clone(),
                window_secs: i64::try_from(window / 1_000_000_000).unwrap_or(i64::MAX),
                bucket,
                observed: observed.0,
                used_percent: used_percent.clone(),
                resets_at: resets_at.clone(),
                host: host.clone(),
            });
        }
    }
    rows.sort_by(|a, b| {
        (
            a.observed,
            a.event,
            a.provider,
            &a.account,
            a.window_secs,
            &a.bucket,
        )
            .cmp(&(
                b.observed,
                b.event,
                b.provider,
                &b.account,
                b.window_secs,
                &b.bucket,
            ))
    });
    let observations = rows
        .iter()
        .map(|row| row.event)
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    Ok(HistoryView {
        since,
        until,
        observations,
        rows,
    })
}

/// No interpolation or consumption sum: each allowance has its own denominator.
pub fn show_json(view: &HistoryView, output: &mut Out<'_>) -> Result<()> {
    let rows = view.rows.iter().map(|row| json!({
        "event": format!("{:x}", row.event),
        "provider": row.provider_name,
        "provider_id": format!("{:x}", row.provider),
        "account": row.account,
        "window_secs": row.window_secs,
        "bucket": row.bucket,
        "observed_at": super::tai_ns_to_unix(row.observed),
        "used_percent": row.used_percent,
        "resets_at": row.resets_at.iter().map(|at| super::tai_ns_to_unix(*at)).collect::<Vec<_>>(),
        "host": row.host,
    })).collect::<Vec<_>>();
    output.line(serde_json::to_string_pretty(&json!({
        "since_inclusive": view.since,
        "until_exclusive": view.until,
        "observations": view.observations,
        "rows": rows,
        "note": "Recorded readings, not usage totals. Missing observations are not zero usage. Compare only within the same provider/account/window/bucket/reset cohort; do not sum overlapping allowances."
    }))?)
}
