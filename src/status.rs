//! Canonical collection-native Status events and read semantics.
//!
//! Status is an immutable event set. Every event is the intrinsic identity of
//! its exact `(window, text handle, point timestamp)` record. The current view
//! is a pure maximum over `(timestamp, event id)`, so collection union cannot
//! make iteration order or process arrival order observable.

pub mod cli;
pub mod mcp;
pub mod operations;
pub mod render;

pub use operations::{SetStatus, Status, StatusEntry, StatusHistory, WindowStatus};

use std::collections::BTreeMap;

use anybytes::View;
use anyhow::{anyhow, bail, Context, Result};
use triblespace::core::metadata;
use triblespace::core::query::TriblePattern;
use triblespace::core::repo::BlobStoreGet;
use triblespace::prelude::*;

use crate::schemas::status::{status, KIND_STATUS_UPDATE};

pub type TextHandle = Inline<inlineencodings::Handle<blobencodings::UTF8String>>;
pub type IntervalValue = Inline<inlineencodings::NsTAIInterval>;

/// One complete immutable Status event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StatusRow {
    pub event: Id,
    pub window: Id,
    pub text: TextHandle,
    pub at: IntervalValue,
}

/// Decode a Status timestamp and require its interval to name one point.
pub fn point_timestamp(interval: IntervalValue) -> Result<i128> {
    let (lower, upper): (i128, i128) = interval
        .try_from_inline()
        .map_err(|error| anyhow!("decode Status timestamp: {error:?}"))?;
    if lower != upper {
        bail!("Status timestamp must be a point interval");
    }
    Ok(lower)
}

/// Build the canonical intrinsic record around an already-stored text handle.
///
/// The stopped-world transform uses this same constructor as the live writer,
/// so migration cannot accidentally introduce a second identity protocol.
pub fn status_record(window: Id, text: TextHandle, at: IntervalValue) -> Fragment {
    entity! {
        metadata::tag: &KIND_STATUS_UPDATE,
        status::window: window,
        status::text: text,
        metadata::created_at: at,
    }
}

/// Build one intrinsic immutable point-valued Status event and carry its text
/// attachment across the collection publication boundary.
pub fn status_fragment(window: Id, text: &str, at: IntervalValue) -> Result<Fragment> {
    point_timestamp(at)?;
    let mut fragment = Fragment::empty();
    let text: TextHandle = fragment.put(text.to_owned());
    fragment += status_record(window, text, at);
    Ok(fragment)
}

/// Project every complete Status event through one typed declarative query.
///
/// Missing or undecodable fields simply do not inhabit this read type.
/// Repeated fields retain their open-world bag semantics instead of making an
/// unrelated historical row poison every ordinary Status observation.
pub fn load_status_rows<P>(facts: &P) -> Result<Vec<StatusRow>>
where
    P: TriblePattern + ?Sized,
{
    Ok(find!(
        (event: Id, window: Id, text: TextHandle, at: IntervalValue),
        pattern!(facts, [{ ?event @
            metadata::tag: &KIND_STATUS_UPDATE,
            status::window: ?window,
            status::text: ?text,
            metadata::created_at: ?at,
        }])
    )
    .map(|(event, window, text, at)| StatusRow {
        event,
        window,
        text,
        at,
    })
    .collect())
}

/// Canonical ordering coordinate for one immutable event.
pub fn event_key(row: &StatusRow) -> Result<(i128, Id)> {
    Ok((
        point_timestamp(row.at)
            .with_context(|| format!("validate timestamp on Status event {}", row.event))?,
        row.event,
    ))
}

/// Resolve one deterministic current event per window.
///
/// A larger point timestamp wins. Equal-time events coexist, and the larger
/// intrinsic event id breaks the tie. This is a pure maximum in a total order:
/// it is permutation-independent and commutes with set union.
pub fn latest_per_window(
    rows: impl IntoIterator<Item = StatusRow>,
) -> Result<BTreeMap<Id, StatusRow>> {
    let mut latest = BTreeMap::<Id, ((i128, Id), StatusRow)>::new();
    for row in rows {
        let key = event_key(&row)?;
        let replace = latest
            .get(&row.window)
            .is_none_or(|(current, _)| key > *current);
        if replace {
            latest.insert(row.window, (key, row));
        }
    }
    Ok(latest
        .into_iter()
        .map(|(window, (_, row))| (window, row))
        .collect())
}

fn load_text_from<Store>(reader: &Store, handle: TextHandle) -> Result<String>
where
    Store: BlobStoreGet + ?Sized,
{
    let text: View<str> = reader
        .get(handle)
        .with_context(|| format!("read Status text payload {}", hex::encode(handle.raw)))?;
    Ok(text.to_string())
}

/// Strictly read one Status text attachment.
pub fn read_text<Store>(reader: &Store, handle: TextHandle) -> Result<String>
where
    Store: BlobStoreGet + ?Sized,
{
    load_text_from(reader, handle)
}

#[cfg(test)]
mod tests {
    use anybytes::Bytes;
    use hifitime::Epoch;
    use triblespace::core::blob::Blob;
    use triblespace::core::repo::memoryrepo::MemoryRepo;

    use super::*;

    fn id(byte: u8) -> Id {
        Id::new([byte; 16]).unwrap()
    }

    fn at(seconds: f64) -> IntervalValue {
        let epoch = Epoch::from_unix_seconds(seconds);
        (epoch, epoch).try_to_inline().unwrap()
    }

    fn row(event: u8, window: Id, seconds: f64) -> StatusRow {
        StatusRow {
            event: id(event),
            window,
            text: Inline::new([event; 32]),
            at: at(seconds),
        }
    }

    #[test]
    fn published_status_ids_remain_byte_exact() {
        assert_eq!(
            KIND_STATUS_UPDATE,
            Id::from_hex("1622DB88E9D9B455EEE1E82470E6730C").unwrap()
        );
        assert_eq!(
            status::window.id(),
            Id::from_hex("51D3C4DEDA7BCFCCA4C3D85FFB7CCFAC").unwrap()
        );
        assert_eq!(
            status::text.id(),
            Id::from_hex("0DB5E52B99D75A09E666718147C45208").unwrap()
        );
        assert_eq!(
            crate::schemas::status::DEFAULT_SCOPE_ID,
            Id::from_hex("5C563832935FD4CFC726D63D2631DC5D").unwrap()
        );
    }

    #[test]
    fn intrinsic_constructor_is_order_stable_and_exact_replay_is_identical() {
        let first = status_fragment(id(1), "mapping the lattice", at(10.0)).unwrap();
        let second = status_fragment(id(1), "mapping the lattice", at(10.0)).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.root(), second.root());
        assert_eq!(first.facts().len(), 4);
    }

    #[test]
    fn constructor_rejects_non_point_time() {
        let interval = (
            Epoch::from_unix_seconds(10.0),
            Epoch::from_unix_seconds(11.0),
        )
            .try_to_inline()
            .unwrap();
        assert!(format!(
            "{:#}",
            status_fragment(id(1), "range", interval).unwrap_err()
        )
        .contains("must be a point interval"));
    }

    #[test]
    fn read_text_rejects_missing_and_malformed_text_payloads() {
        let missing: TextHandle = Inline::new([0xA5; 32]);
        let mut storage = MemoryRepo::default();
        let reader = storage.snapshot().unwrap();
        assert!(format!("{:#}", read_text(&reader, missing).unwrap_err())
            .contains("read Status text payload"));

        let invalid = Blob::<blobencodings::UTF8String>::new(Bytes::from(vec![0xFF]));
        let invalid_handle = invalid.get_handle();
        storage.blobs.insert(invalid);
        let reader = storage.snapshot().unwrap();
        assert!(read_text(&reader, invalid_handle).is_err());
    }

    #[test]
    fn typed_reads_ignore_unrelated_facts() {
        let mut storage = MemoryRepo::default();
        let text: TextHandle = storage.put("legacy".to_owned()).unwrap();
        let legacy = ufoid();
        let random = entity! { &legacy @
            metadata::tag: &KIND_STATUS_UPDATE,
            status::window: id(5),
            status::text: text,
            metadata::created_at: at(40.0),
        };
        assert_eq!(load_status_rows(random.facts()).unwrap().len(), 1);

        let mut extra = status_record(id(6), text, at(41.0));
        let event = extra.root().unwrap();
        extra += entity! { ExclusiveId::force_ref(&event) @ metadata::tag: &id(7) };
        assert_eq!(load_status_rows(extra.facts()).unwrap().len(), 1);

        let mut outside = status_record(id(8), text, at(42.0));
        outside += entity! { metadata::tag: &id(9) };
        assert_eq!(load_status_rows(outside.facts()).unwrap().len(), 1);
    }

    #[test]
    fn incomplete_events_are_skipped_and_repeated_fields_do_not_fail() {
        let mut storage = MemoryRepo::default();
        let text: TextHandle = storage.put("kept".to_owned()).unwrap();
        let missing = ufoid();
        let incomplete = entity! { &missing @
            metadata::tag: &KIND_STATUS_UPDATE,
            status::window: id(16),
            metadata::created_at: at(70.0),
        };
        assert!(load_status_rows(incomplete.facts()).unwrap().is_empty());

        let repeated = ufoid();
        let doubled = entity! { &repeated @
            metadata::tag: &KIND_STATUS_UPDATE,
            status::window: id(17),
            status::window: id(18),
            status::text: text,
            metadata::created_at: at(71.0),
        };
        assert_eq!(load_status_rows(doubled.facts()).unwrap().len(), 2);
    }

    #[test]
    fn latest_is_permutation_independent_and_equal_time_uses_event_id() {
        let window = id(10);
        let low = row(11, window, 50.0);
        let high = row(12, window, 50.0);
        for rows in [vec![low, high], vec![high, low]] {
            assert_eq!(latest_per_window(rows).unwrap()[&window], high);
        }
    }

    #[test]
    fn later_time_dominates_any_equal_time_pair() {
        let window = id(13);
        let low = row(14, window, 60.0);
        let high = row(15, window, 60.0);
        let later = row(1, window, 61.0);
        for rows in [
            vec![low, high, later],
            vec![later, high, low],
            vec![high, later, low],
        ] {
            assert_eq!(latest_per_window(rows).unwrap()[&window], later);
        }
    }
}
