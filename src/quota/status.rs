//! The providers' own public status pages, one immutable observation each.
//!
//! Both providers publish a Statuspage v2 `summary.json`, so one reader
//! serves both. What it records is the provider's claim about itself, not a
//! measurement of ours: the overall indicator and description, every
//! component the page does not call "operational" with the status it gives,
//! every incident it lists with its status, when the page was read, and from
//! which host. A page that cannot be fetched or parsed records nothing; there
//! is no fallback reading.
//!
//! Components and incidents are child entities with intrinsic ids derived
//! from their name and status, so the same component in the same state is the
//! same entity across observations. That is idempotence and nothing more.

use std::collections::BTreeMap;
use std::io::Read;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use triblespace::core::inline::encodings::time::Lower;
use triblespace::core::metadata;
use triblespace::core::query::TriblePattern;
use triblespace::prelude::*;

use super::{known_provider_name, unix_point, TextHandle};
use crate::schemas::quota::{
    provider_status, quota, KIND_PROVIDER_STATUS_OBSERVATION, PROVIDER_CLAUDE, PROVIDER_CODEX,
};

/// Each provider's public Statuspage v2 summary. status.anthropic.com
/// redirects to status.claude.com.
pub const STATUS_PAGES: [(Id, &str); 2] = [
    (
        PROVIDER_CLAUDE,
        "https://status.claude.com/api/v2/summary.json",
    ),
    (
        PROVIDER_CODEX,
        "https://status.openai.com/api/v2/summary.json",
    ),
];

/// How long one page fetch may take, connection included.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// Largest summary accepted. Real ones are tens of kilobytes.
pub const MAX_SUMMARY_BYTES: u64 = 4 * 1024 * 1024;

/// A generic agent string: the software, never a person.
const USER_AGENT: &str = concat!("faculties-quota/", env!("CARGO_PKG_VERSION"));

/// The page's word for a component that is fine. Anything else is recorded.
const OPERATIONAL: &str = "operational";

/// A status observation older than this is labelled stale. It is still shown.
pub const STATUS_STALE_AFTER_SECONDS: i64 = 3_600;

/// One named component or incident and the status the page gives it.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct PageItem {
    pub name: String,
    pub status: String,
}

/// One status page reading as parsed, before anything is stored.
#[derive(Clone, Debug, PartialEq)]
pub struct ProviderStatus {
    pub provider: Id,
    pub indicator: String,
    pub description: String,
    /// Components whose status is not "operational", sorted.
    pub components: Vec<PageItem>,
    /// Every incident the summary lists, sorted.
    pub incidents: Vec<PageItem>,
    /// Unix seconds.
    pub observed_at: i64,
    pub host: String,
}

/// The parts of a Statuspage v2 summary this reader needs. Everything else
/// in the document is ignored.
#[derive(Deserialize)]
struct Summary {
    status: SummaryStatus,
    #[serde(default)]
    components: Vec<SummaryItem>,
    #[serde(default)]
    incidents: Vec<SummaryItem>,
}

#[derive(Deserialize)]
struct SummaryStatus {
    indicator: String,
    description: String,
}

#[derive(Deserialize)]
struct SummaryItem {
    name: String,
    status: String,
}

/// Fetch one summary document over HTTPS with a short timeout.
pub fn fetch_summary(url: &str) -> Result<Vec<u8>> {
    let client = reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(FETCH_TIMEOUT)
        .https_only(true)
        .build()
        .context("build http client")?;
    let response = client
        .get(url)
        .send()
        .with_context(|| format!("fetch {url}"))?
        .error_for_status()
        .with_context(|| format!("fetch {url}"))?;
    let mut body = Vec::new();
    response
        .take(MAX_SUMMARY_BYTES + 1)
        .read_to_end(&mut body)
        .with_context(|| format!("read {url}"))?;
    if body.len() as u64 > MAX_SUMMARY_BYTES {
        bail!("{url} is larger than {MAX_SUMMARY_BYTES} bytes");
    }
    Ok(body)
}

/// Parse one summary document into the reading recorded for `provider`.
pub fn parse_summary(
    provider: Id,
    body: &[u8],
    observed_at: i64,
    host: &str,
) -> Result<ProviderStatus> {
    let summary: Summary =
        serde_json::from_slice(body).context("parse Statuspage v2 summary.json")?;
    let mut components: Vec<PageItem> = summary
        .components
        .into_iter()
        .filter(|component| component.status != OPERATIONAL)
        .map(|component| PageItem {
            name: component.name,
            status: component.status,
        })
        .collect();
    components.sort();
    components.dedup();
    let mut incidents: Vec<PageItem> = summary
        .incidents
        .into_iter()
        .map(|incident| PageItem {
            name: incident.name,
            status: incident.status,
        })
        .collect();
    incidents.sort();
    incidents.dedup();
    let status = ProviderStatus {
        provider,
        indicator: summary.status.indicator,
        description: summary.status.description,
        components,
        incidents,
        observed_at,
        host: host.trim().to_owned(),
    };
    validate(&status)?;
    Ok(status)
}

/// Writer-side checks. Readers never apply these: they show what is there.
pub fn validate(status: &ProviderStatus) -> Result<()> {
    if known_provider_name(status.provider).is_none() {
        bail!(
            "provider {:x} is not one this build records",
            status.provider
        );
    }
    if status.indicator.trim().is_empty() {
        bail!("the page's status indicator is empty");
    }
    if status.host.trim().is_empty() {
        bail!("host is empty");
    }
    Ok(())
}

/// Build one status observation and the provider name it is recorded under.
///
/// Returns the fragment to commit and the observation's intrinsic id.
pub fn status_fragment(status: &ProviderStatus) -> Result<(Fragment, Id)> {
    validate(status)?;
    let provider = status.provider;
    let observed_at = unix_point(status.observed_at)?;

    let mut fragment = Fragment::empty();
    let indicator: TextHandle = fragment.put(status.indicator.clone());
    let description: TextHandle = fragment.put(status.description.clone());
    let host: TextHandle = fragment.put(status.host.trim().to_owned());
    let items = |items: &[PageItem]| {
        let mut children = Fragment::empty();
        for item in items {
            let name: TextHandle = children.put(item.name.clone());
            let state: TextHandle = children.put(item.status.clone());
            children += entity! {
                metadata::name: name,
                provider_status::state: state,
            };
        }
        children
    };
    let record = entity! {
        metadata::tag: &KIND_PROVIDER_STATUS_OBSERVATION,
        quota::provider: provider,
        provider_status::indicator: indicator,
        provider_status::description: description,
        provider_status::component*: items(&status.components),
        provider_status::incident*: items(&status.incidents),
        metadata::created_at: observed_at,
        quota::host: host,
    };
    let event = record
        .root()
        .expect("a provider status observation has one intrinsic root");
    fragment += record;

    let name = known_provider_name(provider).expect("validated provider");
    let name: TextHandle = fragment.put(name.to_owned());
    fragment += entity! { ExclusiveId::force_ref(&provider) @ metadata::name: name };
    Ok((fragment, event))
}

/// One status observation that carries a provider and an observed time.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StatusRow {
    pub event: Id,
    pub provider: Id,
    /// TAI nanoseconds (the interval's lower bound).
    pub observed: i128,
}

/// Every status observation that carries the fields it is grouped and
/// ordered by, through one typed query.
pub fn status_rows<P>(facts: &P) -> Vec<StatusRow>
where
    P: TriblePattern + ?Sized,
{
    find!(
        (event: Id, provider: Id, observed: Lower),
        pattern!(facts, [{ ?event @
            metadata::tag: &KIND_PROVIDER_STATUS_OBSERVATION,
            quota::provider: ?provider,
            metadata::created_at: ?observed,
        }])
    )
    .map(|(event, provider, observed)| StatusRow {
        event,
        provider,
        observed: observed.0,
    })
    .collect()
}

/// The latest status observation per provider: larger observed time wins,
/// the larger event id breaks a tie. A pure maximum in a total order.
pub fn latest_status_per_provider(
    rows: impl IntoIterator<Item = StatusRow>,
) -> BTreeMap<Id, StatusRow> {
    let mut latest = BTreeMap::<Id, StatusRow>::new();
    for row in rows {
        let replace = latest
            .get(&row.provider)
            .is_none_or(|current| (row.observed, row.event) > (current.observed, current.event));
        if replace {
            latest.insert(row.provider, row);
        }
    }
    latest
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPENAI_OUTAGE: &[u8] = include_bytes!("fixtures/openai_outage.json");
    const CLAUDE_OPERATIONAL: &[u8] = include_bytes!("fixtures/claude_operational.json");

    fn item(name: &str, status: &str) -> PageItem {
        PageItem {
            name: name.to_owned(),
            status: status.to_owned(),
        }
    }

    #[test]
    fn published_status_ids_remain_byte_exact() {
        assert_eq!(
            KIND_PROVIDER_STATUS_OBSERVATION,
            Id::from_hex("690BFDAA0BA82913B1C9D08ACDECAE78").unwrap()
        );
    }

    #[test]
    fn an_outage_summary_keeps_what_is_not_operational() {
        let status = parse_summary(PROVIDER_CODEX, OPENAI_OUTAGE, 100, "sky").unwrap();
        assert_eq!(status.indicator, "critical");
        assert_eq!(status.description, "Major System Outage");
        assert_eq!(
            status.components,
            vec![item("ChatGPT", "full_outage"), item("Codex", "full_outage")]
        );
        assert_eq!(
            status.incidents,
            vec![item("Issues with Codex", "identified")]
        );
        assert_eq!(status.observed_at, 100);
        assert_eq!(status.host, "sky");
    }

    #[test]
    fn an_operational_summary_has_no_components_or_incidents() {
        let status = parse_summary(PROVIDER_CLAUDE, CLAUDE_OPERATIONAL, 100, "sky").unwrap();
        assert_eq!(status.indicator, "none");
        assert_eq!(status.description, "All Systems Operational");
        assert!(status.components.is_empty());
        assert!(status.incidents.is_empty());
    }

    #[test]
    fn a_page_without_an_indicator_is_not_a_reading() {
        assert!(parse_summary(PROVIDER_CLAUDE, b"<html>maintenance</html>", 100, "sky").is_err());
        assert!(parse_summary(PROVIDER_CLAUDE, br#"{"components": []}"#, 100, "sky").is_err());
        assert!(parse_summary(
            PROVIDER_CLAUDE,
            br#"{"status": {"indicator": "", "description": ""}}"#,
            100,
            "sky"
        )
        .is_err());
    }

    #[test]
    fn a_summary_reads_back_through_the_typed_queries() {
        let status = parse_summary(PROVIDER_CODEX, OPENAI_OUTAGE, 100, "sky").unwrap();
        let (fragment, event) = status_fragment(&status).unwrap();
        let (again, again_event) = status_fragment(&status).unwrap();
        assert_eq!(fragment, again);
        assert_eq!(event, again_event);
        let facts = fragment.facts();
        let rows = status_rows(facts);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].event, event);
        assert_eq!(rows[0].provider, PROVIDER_CODEX);
        let components = find!(
            (name: TextHandle, state: TextHandle),
            pattern!(facts, [
                { event @ provider_status::component: _?child },
                { _?child @ metadata::name: ?name, provider_status::state: ?state }
            ])
        )
        .count();
        assert_eq!(components, 2);
        let incidents = find!(
            (name: TextHandle, state: TextHandle),
            pattern!(facts, [
                { event @ provider_status::incident: _?child },
                { _?child @ metadata::name: ?name, provider_status::state: ?state }
            ])
        )
        .count();
        assert_eq!(incidents, 1);
    }
}
