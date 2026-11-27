//! A bounded projection of recorded readings, never an inferred usage ledger.

use anyhow::Result;
use serde_json::json;
use triblespace::core::inline::encodings::time::Lower;
use triblespace::core::metadata;
use triblespace::core::query::rangeconstraint::value_range;
use triblespace::prelude::*;

use super::operations::{provider_name, read_text};
use super::{group_observations, quota, ObservationRow, QuotaGroup, TextHandle};
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

pub(crate) fn rows_in_range(
    facts: &FactArchive,
    since: i64,
    until: i64,
) -> Result<impl Iterator<Item = ObservationRow> + '_> {
    anyhow::ensure!(since < until, "since must be before until");
    // `value_range` has inclusive raw-byte bounds (core/query/rangeconstraint.rs).
    // NsTAIInterval orders by lower then upper (core/inline/encodings/time.rs).
    // Start with the native encoder, then make a range sentinel just below
    // `until`, including every upper half. A point-valued max would wrongly
    // exclude nonpoint intervals with the same selected lower bound.
    let min = super::unix_point(since)?;
    let mut max = super::unix_point(until)?;
    let encoded_lower = u128::from_be_bytes(max.raw[..16].try_into()?);
    let below_until = encoded_lower
        .checked_sub(1)
        .ok_or_else(|| anyhow::anyhow!("until precedes the supported time range"))?;
    max.raw[..16].copy_from_slice(&below_until.to_be_bytes());
    max.raw[16..].fill(255);
    let rows = find!(
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
    );
    Ok(group_observations(facts, rows))
}

pub(super) fn observe(
    facts: &FactArchive,
    snapshot: &AcquiringReader<FacultySnapshot>,
    since: i64,
    until: i64,
) -> Result<HistoryView> {
    let mut rows = Vec::new();
    for row in rows_in_range(facts, since, until)? {
        let event = row.event;
        let key = row.key;
        let (used_percent, resets_at) = super::reading_values(facts, event);
        let mut resets_at = resets_at.collect::<Vec<_>>();
        resets_at.sort_unstable();
        let mut host = find!(
            host: TextHandle,
            pattern!(facts, [{ event @ quota::host: ?host }])
        )
        .map(|host| read_text(snapshot, host, "host"))
        .collect::<Result<Vec<_>>>()?;
        host.sort();
        rows.push(QuotaGroup {
            event,
            provider: key.provider,
            provider_name: provider_name(facts, snapshot, key.provider)?,
            account: read_text(snapshot, key.account, "account")?,
            window_secs: i64::try_from(key.window / 1_000_000_000).unwrap_or(i64::MAX),
            bucket: key
                .bucket
                .map(|handle| read_text(snapshot, handle, "bucket"))
                .transpose()?,
            observed: row.observed,
            used_percent: used_percent.collect(),
            resets_at,
            host,
        });
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
