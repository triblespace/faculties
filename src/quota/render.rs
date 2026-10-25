//! Presentation of already-read Quota observations. No storage reads occur here.

use chrono::{DateTime, Utc};
use chrono_tz::Europe::Berlin;
use serde_json::{json, Value};

use super::{QuotaGroup, QuotaView, RecordedObservation};
use crate::out::Out;
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
        "recorded {:x}: {provider} {} acct {} used {} resets {} observed {} host {}",
        receipt.event,
        format_window(observation.window_secs as i64),
        short_account(&observation.account),
        format_percent(observation.used_percent),
        format_instant(observation.resets_at),
        format_instant(observation.observed_at),
        observation.host.trim(),
    ))
}

pub fn show(view: &QuotaView, output: &mut Out<'_>) -> Result<()> {
    output.line(format!(
        "quota: {} observations examined, {} groups shown (the latest per provider, account \
         and window, by observed time)",
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
        return output.line("- (no observations recorded)");
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
            "{} {} acct {}: used {}, elapsed {}, {}; age {}{}; resets {}; host {}",
            provider_label(group),
            format_window(group.window_secs),
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
    Ok(())
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
    let document = json!({
        "now": quota::tai_ns_to_unix(view.now),
        "examined": view.examined,
        "shown": view.groups.len(),
        "pace_margin_points": PACE_MARGIN_POINTS,
        "stale_after_secs": STALE_AFTER_SECONDS,
        "groups": groups,
    });
    output.line(serde_json::to_string_pretty(&document)?)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
