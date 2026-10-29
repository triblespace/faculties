//! Presentation of already-read Quota observations. No storage reads occur here.

use chrono::{DateTime, Utc};
use chrono_tz::Europe::Berlin;
use serde_json::{json, Value};

use super::{ProviderStatusView, QuotaGroup, QuotaView, RecordedObservation, StatusOutcome};
use crate::out::Out;
use crate::quota::status::{PageItem, STATUS_STALE_AFTER_SECONDS};
use crate::quota::{self, Pace, PACE_MARGIN_POINTS, STALE_AFTER_SECONDS};
use anyhow::Result;

const NANOS_PER_SECOND: i128 = 1_000_000_000;

/// Compact age like "42s" / "17m" / "7h05m" / "3d".
fn format_age(seconds: i64) -> String {
    let seconds = seconds.max(0);
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3_600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h{:02}m", seconds / 3_600, seconds % 3_600 / 60)
    } else {
        format!("{}d", seconds / 86_400)
    }
}

/// Window length in the largest unit that divides it: "5h", "7d", "90m".
fn format_window(seconds: i64) -> String {
    if seconds > 0 && seconds % 86_400 == 0 {
        format!("{}d", seconds / 86_400)
    } else if seconds > 0 && seconds % 3_600 == 0 {
        format!("{}h", seconds / 3_600)
    } else if seconds > 0 && seconds % 60 == 0 {
        format!("{}m", seconds / 60)
    } else {
        format!("{seconds}s")
    }
}

/// A percentage as reported, without inventing precision: "3", "7.5", "12.34".
fn format_percent(value: f64) -> String {
    let text = format!("{value:.2}");
    let text = text.trim_end_matches('0').trim_end_matches('.');
    format!("{text}%")
}

fn utc(seconds: i64) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp(seconds, 0)
}

fn format_instant(seconds: i64) -> String {
    match utc(seconds) {
        Some(at) => format!(
            "{} UTC = {}",
            at.format("%Y-%m-%d %H:%M"),
            at.with_timezone(&Berlin).format("%a %Y-%m-%d %H:%M %Z")
        ),
        None => format!("unix {seconds}"),
    }
}

fn provider_label(group: &QuotaGroup) -> String {
    group
        .provider_name
        .clone()
        .unwrap_or_else(|| format!("{:x}", group.provider))
}

fn status_provider_label(status: &ProviderStatusView) -> String {
    status
        .provider_name
        .clone()
        .unwrap_or_else(|| format!("{:x}", status.provider))
}

fn status_age_seconds(now: i128, status: &ProviderStatusView) -> i64 {
    i64::try_from((now - status.observed) / NANOS_PER_SECOND).unwrap_or(i64::MAX)
}

/// "Codex full_outage, ChatGPT partial_outage".
fn page_items(items: &[PageItem], quote: bool) -> String {
    items
        .iter()
        .map(|item| {
            if quote {
                format!("{:?} {}", item.name, item.status)
            } else {
                format!("{} {}", item.name, item.status)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// " bucket <name>" when the observation names an allowance, else nothing.
fn bucket_label(bucket: Option<&str>) -> String {
    bucket
        .map(|bucket| format!(" bucket {bucket}"))
        .unwrap_or_default()
}

fn short_account(account: &str) -> String {
    if account.chars().count() > 12 {
        format!("{}…", account.chars().take(12).collect::<String>())
    } else {
        account.to_owned()
    }
}

/// Everything derived for one group at the view's instant.
struct Derived {
    age_seconds: i64,
    stale: bool,
    /// Present only when exactly one reset time is recorded.
    elapsed: Option<f64>,
    /// Present only when exactly one used value and one reset time exist.
    pace: Option<Pace>,
}

fn derive(now: i128, group: &QuotaGroup) -> Derived {
    let age_seconds = i64::try_from((now - group.observed) / NANOS_PER_SECOND).unwrap_or(i64::MAX);
    let window = i128::from(group.window_secs) * NANOS_PER_SECOND;
    let elapsed = match group.resets_at.as_slice() {
        [resets] => Some(quota::elapsed_percent(now, *resets, window)),
        _ => None,
    };
    let pace = match (group.used_percent.as_slice(), group.resets_at.as_slice()) {
        ([used], [resets]) => Some(quota::pace(now, *used, *resets, window)),
        _ => None,
    };
    Derived {
        age_seconds,
        stale: age_seconds > STALE_AFTER_SECONDS,
        elapsed,
        pace,
    }
}

fn joined<T>(values: &[T], format: impl Fn(&T) -> String) -> String {
    match values {
        [] => "missing".to_owned(),
        [one] => format(one),
        many => format!(
            "{} ({} values)",
            many.iter().map(&format).collect::<Vec<_>>().join("|"),
            many.len()
        ),
    }
}

pub fn record(receipt: &RecordedObservation, output: &mut Out<'_>) -> Result<()> {
    let observation = &receipt.observation;
    let provider = quota::known_provider_name(observation.provider)
        .map(str::to_owned)
        .unwrap_or_else(|| format!("{:x}", observation.provider));
    output.line(format!(
        "recorded {:x}: {provider} {}{} acct {} used {} resets {} observed {} host {}",
        receipt.event,
        format_window(observation.window_secs as i64),
        bucket_label(observation.bucket.as_deref()),
        short_account(&observation.account),
        format_percent(observation.used_percent),
        format_instant(observation.resets_at),
        format_instant(observation.observed_at),
        observation.host.trim(),
    ))
}

pub fn show(view: &QuotaView, output: &mut Out<'_>) -> Result<()> {
    output.line(format!(
        "quota: {} observations examined, {} groups shown (the latest per provider, account, \
         window and bucket, by observed time)",
        view.examined,
        view.groups.len()
    ))?;
    output.line(format!(
        "pace is a planning heuristic relative to the window: used% against elapsed% of it, \
         under/over pace when they differ by more than {PACE_MARGIN_POINTS} points, otherwise \
         on pace. age is the age of the last recorded observation; STALE after {}h.",
        STALE_AFTER_SECONDS / 3_600
    ))?;
    if view.groups.is_empty() {
        output.line("- (no observations recorded)")?;
    }
    for group in &view.groups {
        let derived = derive(view.now, group);
        let elapsed = derived
            .elapsed
            .map(|value| format!("{value:.0}%"))
            .unwrap_or_else(|| "missing".to_owned());
        let pace = derived
            .pace
            .map(|pace| pace.label())
            .unwrap_or("no verdict");
        let stale = if derived.stale { " STALE" } else { "" };
        output.line(format!(
            "{} {}{} acct {}: used {}, elapsed {}, {}; age {}{}; resets {}; host {}",
            provider_label(group),
            format_window(group.window_secs),
            bucket_label(group.bucket.as_deref()),
            short_account(&group.account),
            joined(&group.used_percent, |used| format_percent(*used)),
            elapsed,
            pace,
            format_age(derived.age_seconds),
            stale,
            joined(&group.resets_at, |at| format_instant(
                quota::tai_ns_to_unix(*at)
            )),
            joined(&group.host, String::clone),
        ))?;
    }
    provider_statuses(view, output)
}

/// The latest reading of each provider's own status page. Nothing is printed
/// when none was ever recorded, so the quota output stays as it was.
fn provider_statuses(view: &QuotaView, output: &mut Out<'_>) -> Result<()> {
    if view.status_examined == 0 {
        return Ok(());
    }
    output.line(format!(
        "provider status: {} observations examined, {} providers shown (the latest per \
         provider, by observed time). Each is the provider's own status page, its claim about \
         itself, not our measurement; STALE after {}h.",
        view.status_examined,
        view.statuses.len(),
        STATUS_STALE_AFTER_SECONDS / 3_600
    ))?;
    for status in &view.statuses {
        let age = status_age_seconds(view.now, status);
        let mut line = format!(
            "{} status {} ({})",
            status_provider_label(status),
            joined(&status.indicator, String::clone),
            joined(&status.description, String::clone),
        );
        if !status.components.is_empty() {
            line.push_str(&format!(
                "; not operational: {}",
                page_items(&status.components, false)
            ));
        }
        if !status.incidents.is_empty() {
            line.push_str(&format!(
                "; incidents: {}",
                page_items(&status.incidents, true)
            ));
        }
        line.push_str(&format!(
            "; age {}{}; host {}; provider's own status page",
            format_age(age),
            if age > STATUS_STALE_AFTER_SECONDS {
                " STALE"
            } else {
                ""
            },
            joined(&status.host, String::clone),
        ));
        output.line(line)?;
    }
    Ok(())
}

/// One line per status page: what was recorded, or a loud failure.
pub fn provider_status(outcomes: &[StatusOutcome], output: &mut Out<'_>) -> Result<()> {
    for outcome in outcomes {
        let provider = quota::known_provider_name(outcome.provider)
            .map(str::to_owned)
            .unwrap_or_else(|| format!("{:x}", outcome.provider));
        match &outcome.result {
            Ok(receipt) => {
                let status = &receipt.status;
                output.line(format!(
                    "recorded {:x}: {provider} status {} ({}); {} components not operational, \
                     {} incidents; observed {}; host {}; provider's own status page {}",
                    receipt.event,
                    status.indicator,
                    status.description,
                    status.components.len(),
                    status.incidents.len(),
                    format_instant(status.observed_at),
                    status.host,
                    outcome.url,
                ))?;
            }
            Err(error) => output.line(format!(
                "FAILED {provider}: {error:#}; nothing recorded for {provider} (status page {})",
                outcome.url
            ))?,
        }
    }
    Ok(())
}

/// The compact block Orient's overview carries: one line per group, no
/// preamble. Empty when nothing is recorded, so the overview omits it. The
/// account appears only when the view holds more than one.
pub fn section(view: &QuotaView) -> String {
    if view.groups.is_empty() {
        return String::new();
    }
    let accounts: std::collections::BTreeSet<&str> = view
        .groups
        .iter()
        .map(|group| group.account.as_str())
        .collect();
    let mut text = String::from("Quota:\n");
    for group in &view.groups {
        let derived = derive(view.now, group);
        let account = if accounts.len() > 1 {
            format!(" acct {}", short_account(&group.account))
        } else {
            String::new()
        };
        text.push_str(&format!(
            "- {} {}{}{}: used {}, {}; age {}{}; resets {}\n",
            provider_label(group),
            format_window(group.window_secs),
            bucket_label(group.bucket.as_deref()),
            account,
            joined(&group.used_percent, |used| format_percent(*used)),
            derived
                .pace
                .map(|pace| pace.label())
                .unwrap_or("no verdict"),
            format_age(derived.age_seconds),
            if derived.stale { " STALE" } else { "" },
            joined(&group.resets_at, |at| format_instant(
                quota::tai_ns_to_unix(*at)
            )),
        ));
    }
    text
}

pub fn show_json(view: &QuotaView, output: &mut Out<'_>) -> Result<()> {
    fn one_or_many<T: Clone + Into<Value>>(values: &[T]) -> Value {
        match values {
            [] => Value::Null,
            [one] => one.clone().into(),
            many => Value::Array(many.iter().cloned().map(Into::into).collect()),
        }
    }
    let groups: Vec<Value> = view
        .groups
        .iter()
        .map(|group| {
            let derived = derive(view.now, group);
            let resets: Vec<i64> = group
                .resets_at
                .iter()
                .map(|at| quota::tai_ns_to_unix(*at))
                .collect();
            let resets_text: Vec<String> = resets.iter().map(|at| format_instant(*at)).collect();
            json!({
                "event": format!("{:x}", group.event),
                "provider": provider_label(group),
                "provider_id": format!("{:x}", group.provider),
                "account": group.account,
                "window_secs": group.window_secs,
                "bucket": group.bucket,
                "used_percent": one_or_many(&group.used_percent),
                "elapsed_percent": derived.elapsed,
                "pace": derived.pace.map(Pace::label),
                "observed_at": quota::tai_ns_to_unix(group.observed),
                "age_secs": derived.age_seconds,
                "stale": derived.stale,
                "resets_at": one_or_many(&resets),
                "resets_at_text": one_or_many(&resets_text),
                "host": one_or_many(&group.host),
            })
        })
        .collect();
    let page_items = |items: &[PageItem]| -> Vec<Value> {
        items
            .iter()
            .map(|item| json!({ "name": item.name, "status": item.status }))
            .collect()
    };
    let statuses: Vec<Value> = view
        .statuses
        .iter()
        .map(|status| {
            let age = status_age_seconds(view.now, status);
            json!({
                "event": format!("{:x}", status.event),
                "provider": status_provider_label(status),
                "provider_id": format!("{:x}", status.provider),
                "source": "provider's own status page",
                "indicator": one_or_many(&status.indicator),
                "description": one_or_many(&status.description),
                "components_not_operational": page_items(&status.components),
                "incidents": page_items(&status.incidents),
                "observed_at": quota::tai_ns_to_unix(status.observed),
                "age_secs": age,
                "stale": age > STATUS_STALE_AFTER_SECONDS,
                "host": one_or_many(&status.host),
            })
        })
        .collect();
    let document = json!({
        "now": quota::tai_ns_to_unix(view.now),
        "examined": view.examined,
        "shown": view.groups.len(),
        "pace_margin_points": PACE_MARGIN_POINTS,
        "stale_after_secs": STALE_AFTER_SECONDS,
        "groups": groups,
        "status_examined": view.status_examined,
        "status_stale_after_secs": STATUS_STALE_AFTER_SECONDS,
        "provider_status": statuses,
    });
    output.line(serde_json::to_string_pretty(&document)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use triblespace::macros::id_hex;

    #[test]
    fn small_formatters_say_what_they_mean() {
        assert_eq!(format_window(18_000), "5h");
        assert_eq!(format_window(604_800), "7d");
        assert_eq!(format_window(5_400), "90m");
        assert_eq!(format_window(61), "61s");
        assert_eq!(format_percent(3.0), "3%");
        assert_eq!(format_percent(7.5), "7.5%");
        assert_eq!(format_percent(12.345), "12.35%");
        assert_eq!(format_age(25_500), "7h05m");
        // 2026-10-25 01:00 UTC is the CEST -> CET switch; just after it is CET.
        assert_eq!(
            format_instant(1_792_894_800),
            "2026-10-25 02:20 UTC = Sun 2026-10-25 03:20 CET"
        );
        assert_eq!(
            format_instant(1_792_800_000),
            "2026-10-24 00:00 UTC = Sat 2026-10-24 02:00 CEST"
        );
    }

    #[test]
    fn the_orient_section_is_one_line_per_group() {
        const NOW: i64 = 1_792_886_255;
        let group =
            |provider: &str, window_secs, bucket: Option<&str>, used, age, resets_in| QuotaGroup {
                event: id_hex!("11111111111111111111111111111111"),
                provider: id_hex!("22222222222222222222222222222222"),
                provider_name: Some(provider.to_owned()),
                account: "a1b2c3d4".to_owned(),
                window_secs,
                bucket: bucket.map(str::to_owned),
                observed: quota::unix_to_tai_ns(NOW - age),
                used_percent: vec![used],
                resets_at: vec![quota::unix_to_tai_ns(NOW + resets_in)],
                host: vec!["sky".to_owned()],
            };
        let mut view = QuotaView {
            now: quota::unix_to_tai_ns(NOW),
            examined: 2,
            groups: vec![
                group("claude", 18_000, None, 9.0, 60, 2 * 3_600),
                group(
                    "codex",
                    604_800,
                    Some("codex_other"),
                    64.0,
                    7 * 3_600,
                    5 * 86_400,
                ),
            ],
            status_examined: 0,
            statuses: Vec::new(),
        };
        let text = section(&view);
        assert_eq!(
            text,
            "Quota:\n\
             - claude 5h: used 9%, under pace; age 1m; resets 2026-10-25 01:57 UTC = Sun \
             2026-10-25 02:57 CET\n\
             - codex 7d bucket codex_other: used 64%, over pace; age 7h00m STALE; resets \
             2026-10-29 23:57 UTC = Fri 2026-10-30 00:57 CET\n",
            "{text}"
        );
        view.groups[1].account = "f0e1d2c3".to_owned();
        assert!(section(&view).contains("- claude 5h acct a1b2c3d4: used 9%"));
        view.groups.clear();
        assert_eq!(section(&view), "");
    }
}
