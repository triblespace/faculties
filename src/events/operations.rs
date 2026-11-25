//! Events operations over resident input and frozen authorized collections.
#[cfg(test)]
use crate::storage::FactView;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use crate::clock;
#[cfg(test)]
use crate::collection_names::open_configured;
use crate::events::{
    self as events_model, cancellation_fragment, event_fragment, note_fragment, read_text,
    rereading_fragment, unescape_text, EventDraft, EventRow, IntervalValue, TextHandle,
    STATUS_CANCELLED, STATUS_CONFIRMED, STATUS_TENTATIVE, TRANSP_OPAQUE,
};
use crate::schemas::planner::{event, DEFAULT_SCOPE_ID, KIND_EVENT_ID, KIND_EXPORT_ID};
use crate::storage::{AcquiringReader, FactArchive, FacultySnapshot, FacultyStore};
use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone, Utc};
use hifitime::Epoch;
use rrule::{RRule, RRuleSet, Tz, Unvalidated};
use triblespace::core::blob::encodings::simplearchive::SimpleArchive;
use triblespace::core::blob::encodings::succinctarchive::{
    Rank9AcceleratedSuccinctArchiveBlob, SuccinctArchiveBlob,
};
#[cfg(test)]
use triblespace::core::collection::CollectionSnapshotExt;
use triblespace::core::collection::{Collection, CollectionStoreExt};
use triblespace::core::metadata;
use triblespace::core::repo::{BlobStoreGet, SnapshotSource};
use triblespace::prelude::*;

/// Explicit observation window; never a sequence of CLI arguments.
pub type Window = (Epoch, Epoch);
#[derive(Clone, Debug)]
pub struct AddOptions {
    pub summary: String,
    pub start: Epoch,
    pub end: Epoch,
    pub rrule: Option<String>,
    pub location: Option<String>,
    pub status: String,
    pub transp: String,
    pub description: Option<String>,
    pub note: Option<String>,
}
impl AddOptions {
    pub fn new(summary: impl Into<String>, window: Window) -> Self {
        Self {
            summary: summary.into(),
            start: window.0,
            end: window.1,
            rrule: None,
            location: None,
            status: STATUS_CONFIRMED.to_owned(),
            transp: TRANSP_OPAQUE.to_owned(),
            description: None,
            note: None,
        }
    }
    pub fn validate(&self) -> Result<()> {
        validate_window((self.start, self.end))?;
        events_model::validate_label("event summary", &self.summary)?;
        if let Some(rule) = &self.rrule {
            events_model::validate_label("event RRULE", rule)?;
        }
        if let Some(location) = &self.location {
            events_model::validate_label("event location", location)?;
        }
        events_model::validate_status(&self.status)?;
        events_model::validate_transp(&self.transp)
    }
}
#[derive(Clone, Debug)]
pub struct AddedEvent {
    pub event: Id,
    pub uid: String,
    pub note: Option<Id>,
}
#[derive(Clone, Copy, Debug)]
pub struct AddedNote {
    pub event: Id,
    pub note: Id,
}
#[derive(Clone, Copy, Debug)]
pub struct CancellationReceipt {
    pub event: Id,
    pub already_cancelled: bool,
}
#[derive(Clone, Debug)]
pub struct NoteDetail {
    pub row: events_model::NoteRow,
    pub text: String,
}
#[derive(Clone, Debug)]
pub struct EventDetail {
    pub event: EventRow,
    pub summary: String,
    pub rrule: Option<String>,
    pub location: Option<String>,
    pub status: String,
    pub uid: String,
    pub description: Option<String>,
    pub cancelled: bool,
    pub notes: Vec<NoteDetail>,
}
/// A resident calendar. The name is diagnostic metadata, never a path to read.
#[derive(Clone, Copy, Debug)]
pub struct CalendarInput<'a> {
    pub name: &'a str,
    pub text: &'a str,
}
/// How an ingest reads datetimes that carry neither `Z` nor `TZID`.
///
/// RFC 5545 floating times mean "this wall-clock time wherever you are". The
/// Events stores instants, so a floating time needs a zone to become one. With
/// `floating_zone` set, floating times are local times in that zone, and the
/// zone is recorded as the event's TZID so recurrences expand in it. Without
/// it, a floating time is an error naming the event; it is never read as UTC.
///
/// `complete_calendar` names the calendar the single document is a complete
/// export of. Only then does the ingest record which versions the export held
/// (an export observation dated by its latest DTSTAMP), and only then can an
/// event left out of a later export stop being listed. A partial file, such
/// as one invitation, is ingested without it and never removes anything.
#[derive(Clone, Debug, Default)]
pub struct IngestOptions {
    pub floating_zone: Option<chrono_tz::Tz>,
    pub complete_calendar: Option<String>,
}
/// An IANA zone name for [`IngestOptions::floating_zone`].
pub fn parse_floating_zone(name: &str) -> Result<chrono_tz::Tz> {
    name.parse::<chrono_tz::Tz>()
        .map_err(|_| anyhow!("floating zone '{name}' is not an IANA time zone"))
}
#[derive(Clone, Debug)]
pub struct IngestReceipt {
    pub imported: Vec<Id>,
    pub total: usize,
    pub duplicates: usize,
    /// Imported records whose UID and RECURRENCE-ID were already present
    /// with a different definition: a new version beside the old one.
    pub revisions: usize,
    /// Present when the document was ingested as a complete export.
    pub export: Option<ExportReceipt>,
}
/// What an ingest of a complete export recorded about its calendar.
#[derive(Clone, Debug)]
pub struct ExportReceipt {
    pub calendar: String,
    /// The latest DTSTAMP in the document.
    pub exported_at: Epoch,
    /// False when this exact export was already recorded.
    pub recorded: bool,
    /// False when an export of this calendar made later is already recorded;
    /// then this one changes nothing about what is listed.
    pub newest: bool,
    /// Events earlier exports of this calendar held that this one does not.
    /// When `newest`, list and next stop showing them (unless their current
    /// version was modified after this export was made).
    pub absent: usize,
}
#[derive(Clone, Debug)]
pub struct Events {
    storage: crate::storage::Storage,
}

pub fn validate_window((start, end): Window) -> Result<()> {
    if end < start {
        bail!("window end is before its start");
    }
    Ok(())
}
pub fn default_window() -> Window {
    (
        Epoch::from_gregorian_utc(1970, 1, 1, 0, 0, 0, 0),
        Epoch::from_gregorian_utc(2100, 1, 1, 0, 0, 0, 0),
    )
}
/// ISO dates and timezone-free datetimes are UTC, matching stored Events
/// interpretation. A missing end defaults to one day or one hour respectively.
pub fn event_window(from: &str, to: Option<&str>) -> Result<Window> {
    let start = parse_iso8601(from)?;
    let end = match to {
        Some(to) => parse_iso8601(to)?,
        None if is_date_only(from) => start + chrono::Duration::days(1),
        None => start + chrono::Duration::hours(1),
    };
    let window = (chrono_to_epoch(start), chrono_to_epoch(end));
    validate_window(window)?;
    Ok(window)
}
pub fn validate_calendars(documents: &[CalendarInput<'_>]) -> Result<()> {
    if documents.is_empty() {
        bail!("no calendar documents supplied");
    }
    if documents.iter().any(|document| document.name.is_empty()) {
        bail!("calendar document name cannot be empty");
    }
    Ok(())
}

impl Events {
    pub fn new(pile: PathBuf, key: Option<PathBuf>) -> Self {
        Self::with_storage(crate::storage::Storage::new(pile, key))
    }
    pub fn with_storage(storage: crate::storage::Storage) -> Self {
        Self { storage }
    }
    fn storage(&self) -> EventsStorage<'_> {
        EventsStorage {
            storage: &self.storage,
        }
    }
    pub fn add(&self, options: &AddOptions) -> Result<AddedEvent> {
        options.validate()?;
        let uid = format!("{:x}@triblespace", genid().id);
        let mut draft = empty_event_draft(
            uid.clone(),
            options.summary.clone(),
            make_interval(options.start, options.end),
            options.status.clone(),
            options.transp.clone(),
        );
        draft.rrule = options.rrule.clone();
        draft.location = options.location.clone();
        draft.description = options.description.clone();
        let mut fragment = event_fragment(&draft)?;
        let event = fragment.root().expect("event fragment has one root");
        let note = if let Some(note) = &options.note {
            let note = note_fragment(event, note, clock::point_now()?)?;
            let id = note.root().expect("note fragment has one root");
            fragment += note;
            Some(id)
        } else {
            None
        };
        self.storage().update("add event", |_| {
            Ok((Some(fragment), AddedEvent { event, uid, note }))
        })
    }
    pub fn list(&self, window: Window, include_cancelled: bool) -> Result<Vec<Occurrence>> {
        validate_window(window)?;
        self.storage().with_view(|loaded| {
            Ok(collect_occurrences(
                &loaded.reader,
                &loaded.facts,
                window,
                include_cancelled,
            ))
        })
    }
    /// Next occurrence overlapping [after, 2100-01-01]; None means capture now.
    pub fn next(&self, after: Option<Epoch>) -> Result<Option<Occurrence>> {
        let start = match after {
            Some(start) => start,
            None => clock::now()?,
        };
        let window = (start, default_window().1);
        validate_window(window)?;
        self.storage()
            .with_view(|loaded| Ok(next_occurrence(&loaded.reader, &loaded.facts, window)))
    }
    pub fn note(&self, id: &str, text: &str) -> Result<AddedNote> {
        self.storage().update("add event note", |loaded| {
            let event = resolve_event_id(id, &loaded.facts)?;
            let fragment = note_fragment(event, text, clock::point_now()?)?;
            let note = fragment.root().expect("note fragment has one root");
            Ok((Some(fragment), AddedNote { event, note }))
        })
    }
    pub fn show(&self, id: &str) -> Result<EventDetail> {
        self.storage().with_view(|loaded| {
            let event_id = resolve_event_id(id, &loaded.facts)?;
            let event = events_model::event(&loaded.facts, event_id).ok_or_else(|| {
                anyhow!(
                    "event {} has no readable Events projection",
                    fmt_id(event_id)
                )
            })?;
            let uid = read_text(&loaded.reader, event.uid)?;
            let summary = read_text(&loaded.reader, event.summary)?;
            let rrule = event
                .rrule
                .map(|handle| read_text(&loaded.reader, handle))
                .transpose()?;
            let location = event
                .location
                .map(|handle| read_text(&loaded.reader, handle))
                .transpose()?;
            let status = read_text(&loaded.reader, event.status)?;
            let description = event
                .description
                .map(|handle| read_text(&loaded.reader, handle))
                .transpose()?;
            let cancelled = events_model::event_is_cancelled(&loaded.facts, event_id);
            // Notes belong to the lineage: one written on an earlier version
            // still shows on the version that replaced it.
            let mut notes = events_model::lineage_versions(&loaded.facts, &event)
                .into_iter()
                .flat_map(|version| events_model::notes_for_event(&loaded.facts, version.id))
                .collect::<Vec<_>>();
            notes.sort_by_key(|row| (interval_key(row.created_at), row.id));
            let notes = notes
                .into_iter()
                .map(|row| {
                    Ok(NoteDetail {
                        text: read_text(&loaded.reader, row.text)?,
                        row,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(EventDetail {
                event,
                summary,
                rrule,
                location,
                status,
                uid,
                description,
                cancelled,
                notes,
            })
        })
    }
    pub fn cancel(&self, id: &str) -> Result<CancellationReceipt> {
        cancel(self.storage(), id)
    }
    pub fn resolve(&self, prefix: &str) -> Result<Id> {
        self.storage()
            .with_view(|loaded| resolve_event_id(prefix, &loaded.facts))
    }
    /// All resident calendars are staged before one publication. Exact
    /// duplicates are skipped. A changed definition of a known UID is a new
    /// version recorded beside the old one; nothing is replaced or refused,
    /// and readers pick the current version per lineage.
    pub fn ingest(
        &self,
        documents: &[CalendarInput<'_>],
        options: IngestOptions,
    ) -> Result<IngestReceipt> {
        ingest(self.storage(), documents, options)
    }
}
fn cancel(storage: EventsStorage<'_>, id: &str) -> Result<CancellationReceipt> {
    storage.update("cancel event", |loaded| {
        let event = resolve_event_id(id, &loaded.facts)?;
        if events_model::event_is_cancelled(&loaded.facts, event) {
            return Ok((
                None,
                CancellationReceipt {
                    event,
                    already_cancelled: true,
                },
            ));
        }
        Ok((
            Some(cancellation_fragment(event)),
            CancellationReceipt {
                event,
                already_cancelled: false,
            },
        ))
    })
}
#[derive(Clone, Copy)]
struct EventsStorage<'a> {
    storage: &'a crate::storage::Storage,
}

struct LoadedEvents {
    facts: FactArchive,
    reader: AcquiringReader<FacultySnapshot>,
}

impl EventsStorage<'_> {
    fn with_store<T>(
        &self,
        operation: impl FnOnce(
            &mut FacultyStore,
            Collection<SimpleArchive>,
            &ed25519_dalek::SigningKey,
            &LoadedEvents,
            &std::sync::Arc<tokio::runtime::Runtime>,
        ) -> Result<T>,
    ) -> Result<T> {
        self.storage.with_store(|pile, signer, runtime| {
            let result = (|| {
                let source = self.storage.open_collection_read(
                    pile,
                    DEFAULT_SCOPE_ID,
                    signer.verifying_key(),
                    runtime,
                )?;
                let collection_succinct = pile.attach::<SuccinctArchiveBlob>(source, ())?;
                let collection_rank9 = pile
                    .attach::<Rank9AcceleratedSuccinctArchiveBlob>(source, collection_succinct)?;
                // Keep the existing upkeep step, then select one observation
                // whose missing residual bytes the foreground read can fetch.
                runtime
                    .block_on(async {
                        crate::storage::tolerate_own_lag(
                            pile.maintain_attached(collection_succinct, signer).await,
                        )?;
                        crate::storage::tolerate_own_lag(
                            pile.maintain_attached(collection_rank9, signer).await,
                        )
                    })
                    .context("maintain Events fact collection")?;
                let store_snapshot = AcquiringReader::new(
                    pile.snapshot()
                        .context("freeze maintained Events fact collection")?,
                    runtime.clone(),
                );
                let facts = crate::storage::acquire_facts(&store_snapshot, collection_rank9)
                    .context("attach maintained Events fact collection")?;
                let loaded = LoadedEvents {
                    facts,
                    reader: store_snapshot,
                };
                operation(pile, source, signer, &loaded, runtime)
            })();
            result
        })
    }

    fn with_view<T>(&self, operation: impl FnOnce(&LoadedEvents) -> Result<T>) -> Result<T> {
        self.with_store(|_, _, _, loaded, _| operation(loaded))
    }

    fn update<T>(
        &self,
        description: &'static str,
        operation: impl FnOnce(&LoadedEvents) -> Result<(Option<Fragment>, T)>,
    ) -> Result<T> {
        self.with_store(|pile, collection, signer, loaded, runtime| {
            let (fragment, value) = operation(loaded)?;
            if let Some(mut fragment) = fragment {
                fragment.describe_with(entity! { metadata::description: description });
                crate::collection_names::require_command_write_admission_acquiring(
                    pile,
                    collection,
                    signer,
                    "Events",
                    "events list",
                    runtime,
                )?;
                pile.commit(collection, signer, fragment)
                    .context("commit authored Events fragment")?;
                drop(
                    runtime
                        .block_on(crate::storage::ensure_downstream(pile, collection, signer))
                        .context(
                            "Events facts were committed, but ensuring their derived views failed",
                        )?,
                );
            }
            Ok(value)
        })
    }

    /// How many distinct payloads the planner's collection stands on. A
    /// write that changes nothing adds none.
    #[cfg(test)]
    fn payload_count(&self) -> Result<usize> {
        self.storage.with_pile(|pile, signer| {
            let collection = self.storage.open_collection_local(
                pile,
                DEFAULT_SCOPE_ID,
                signer.verifying_key(),
            )?;
            let store_snapshot = pile.snapshot()?;
            Ok(collection.admitted(&store_snapshot)?.len())
        })
    }
}

#[cfg(test)]
fn point_interval(epoch: Epoch) -> IntervalValue {
    (epoch, epoch)
        .try_to_inline()
        .expect("an Epoch point is a valid interval")
}

pub(super) fn epoch_to_chrono_utc(epoch: Epoch) -> Result<DateTime<Utc>> {
    let seconds = epoch.to_unix_seconds();
    if !seconds.is_finite() {
        bail!("Events timestamp is not finite");
    }
    let whole = seconds.floor();
    if whole < i64::MIN as f64 || whole > i64::MAX as f64 {
        bail!("Events timestamp is outside the displayable UTC range");
    }
    let nanos = ((seconds - whole) * 1e9).round().clamp(0.0, 999_999_999.0) as u32;
    Utc.timestamp_opt(whole as i64, nanos)
        .single()
        .ok_or_else(|| anyhow!("Events timestamp is outside the displayable UTC range"))
}

pub fn chrono_to_epoch(datetime: DateTime<Utc>) -> Epoch {
    Epoch::from_unix_seconds(
        datetime.timestamp() as f64 + datetime.timestamp_subsec_nanos() as f64 * 1e-9,
    )
}

fn make_interval(start: Epoch, end: Epoch) -> IntervalValue {
    (start, end)
        .try_to_inline()
        .expect("ordered Epoch endpoints form an interval")
}

pub(super) fn unpack_interval(interval: IntervalValue) -> (Epoch, Epoch) {
    interval
        .try_from_inline()
        .expect("validated Events interval")
}

fn interval_key(interval: IntervalValue) -> i128 {
    let (start, _): (i128, i128) = interval
        .try_from_inline()
        .expect("validated Events interval");
    start
}

pub fn parse_iso8601(input: &str) -> Result<DateTime<Utc>> {
    let input = input.trim();
    if let Ok(datetime) = DateTime::parse_from_rfc3339(input) {
        return Ok(datetime.with_timezone(&Utc));
    }
    if let Ok(datetime) = NaiveDateTime::parse_from_str(input, "%Y-%m-%dT%H:%M:%S") {
        return Ok(Utc.from_utc_datetime(&datetime));
    }
    if let Ok(datetime) = NaiveDateTime::parse_from_str(input, "%Y-%m-%dT%H:%M") {
        return Ok(Utc.from_utc_datetime(&datetime));
    }
    if let Ok(date) = NaiveDate::parse_from_str(input, "%Y-%m-%d") {
        return Ok(Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0).expect("midnight exists")));
    }
    bail!("could not parse '{input}' as an ISO 8601 date, local datetime, or RFC 3339 datetime")
}

pub fn is_date_only(input: &str) -> bool {
    NaiveDate::parse_from_str(input.trim(), "%Y-%m-%d").is_ok()
}

fn fmt_id(id: Id) -> String {
    format!("{id:x}")
}

pub(super) fn fmt_interval(interval: IntervalValue) -> Result<String> {
    let (start, end) = unpack_interval(interval);
    let start = epoch_to_chrono_utc(start)?;
    let end = epoch_to_chrono_utc(end)?;
    let formatted = if start == end {
        start.format("%Y-%m-%d %H:%M UTC").to_string()
    } else if (end - start).num_seconds() == 86_400
        && start.format("%H:%M:%S").to_string() == "00:00:00"
    {
        start.format("%Y-%m-%d (all day)").to_string()
    } else {
        format!(
            "{} → {}",
            start.format("%Y-%m-%d %H:%M"),
            end.format("%Y-%m-%d %H:%M UTC")
        )
    };
    Ok(formatted)
}

fn resolve_event_id<P: TriblePattern + ?Sized>(input: &str, facts: &P) -> Result<Id> {
    crate::resolve_id_prefix(input, events_model::event_ids(facts))
}

fn normalized_status(value: Option<&str>) -> String {
    value.unwrap_or(STATUS_CONFIRMED).to_ascii_uppercase()
}

fn normalized_transp(value: Option<&str>) -> String {
    value.unwrap_or(TRANSP_OPAQUE).to_ascii_uppercase()
}

fn empty_event_draft(
    uid: String,
    summary: String,
    time: IntervalValue,
    status: String,
    transp: String,
) -> EventDraft {
    EventDraft {
        uid,
        summary,
        description: None,
        time,
        rrule: None,
        rdates: BTreeSet::new(),
        exdates: BTreeSet::new(),
        location: None,
        status,
        transp,
        attendees: BTreeSet::new(),
        organizer: None,
        sequence: None,
        tzid: None,
        recurrence_id: None,
        last_modified: None,
    }
}

#[derive(Clone, Debug)]
pub struct Occurrence {
    pub event_id: Id,
    pub start: Epoch,
    pub end: Epoch,
    pub summary: String,
    pub status: String,
    pub location: Option<String>,
}

type EpochInterval = (Epoch, Epoch);

/// The fields this command needs from one event. This is deliberately not a
/// collection-wide read model: it lives only while that event's occurrences
/// are derived.
struct OccurrenceEvent {
    id: Id,
    summary: String,
    time: EpochInterval,
    rrule: Option<String>,
    rdates: Vec<EpochInterval>,
    exdates: Vec<EpochInterval>,
    location: Option<String>,
    status: String,
    /// Zone the RRULE expands in; `None` expands in UTC.
    zone: Option<chrono_tz::Tz>,
    /// Starts of recurring instances replaced by an override event with the
    /// same UID and that RECURRENCE-ID. Queried here, never stored on the
    /// master: the source said "replaced", not "excluded".
    overridden: Vec<[u8; 32]>,
}

fn occurrence_event<R, P>(reader: &R, space: &P, id: Id) -> Option<OccurrenceEvent>
where
    R: BlobStoreGet,
    P: TriblePattern + ?Sized,
{
    find!(
        (
            uid: TextHandle,
            summary: TextHandle,
            time: EpochInterval,
            status: TextHandle,
        ),
        pattern!(space, [{ id @
            metadata::tag: &KIND_EVENT_ID,
            event::ical_uid: ?uid,
            event::summary: ?summary,
            event::time: ?time,
            event::status: ?status,
        }])
    )
    .find_map(|(uid, summary, time, status)| {
        let summary = read_text(reader, summary).ok()?;
        let status = read_text(reader, status).ok()?;
        if !matches!(
            status.as_str(),
            STATUS_CONFIRMED | STATUS_TENTATIVE | STATUS_CANCELLED
        ) || epoch_to_chrono_utc(time.0).is_err()
            || epoch_to_chrono_utc(time.1).is_err()
        {
            return None;
        }
        let rdates: Vec<EpochInterval> = find!(
            value: EpochInterval,
            pattern!(space, [{ id @ event::rdate: ?value }])
        )
        .filter(|(start, end)| {
            epoch_to_chrono_utc(*start).is_ok() && epoch_to_chrono_utc(*end).is_ok()
        })
        .collect();
        let exdates: Vec<EpochInterval> = find!(
            value: EpochInterval,
            pattern!(space, [{ id @ event::exdate: ?value }])
        )
        .filter(|(start, end)| {
            epoch_to_chrono_utc(*start).is_ok() && epoch_to_chrono_utc(*end).is_ok()
        })
        .collect();
        Some(OccurrenceEvent {
            id,
            summary,
            time,
            rrule: match find!(
                value: TextHandle,
                pattern!(space, [{ id @ event::rrule: ?value }])
            )
            .next()
            {
                Some(handle) => Some(read_text(reader, handle).ok()?),
                None => None,
            },
            rdates,
            exdates,
            location: match find!(
                value: TextHandle,
                pattern!(space, [{ id @ event::location: ?value }])
            )
            .next()
            {
                Some(handle) => Some(read_text(reader, handle).ok()?),
                None => None,
            },
            status,
            // A zone text no writer here produces (writers validate it) still
            // has a correct first instant; expand it in UTC rather than drop it.
            zone: find!(
                value: TextHandle,
                pattern!(space, [{ id @ event::tzid: ?value }])
            )
            .next()
            .and_then(|handle| read_text(reader, handle).ok())
            .and_then(|zone| zone.parse::<chrono_tz::Tz>().ok()),
            overridden: if exists!(pattern!(space, [{ id @ event::recurrence_id: _?own }])) {
                Vec::new()
            } else {
                find!(
                    start: IntervalValue,
                    pattern!(space, [{ _?other @
                        metadata::tag: &KIND_EVENT_ID,
                        event::ical_uid: uid,
                        event::recurrence_id: ?start,
                    }])
                )
                .map(|start| start.raw)
                .collect()
            },
        })
    })
}

fn interval_raw((start, end): EpochInterval) -> [u8; 32] {
    make_interval(start, end).raw
}

fn is_excluded(row: &OccurrenceEvent, interval: EpochInterval) -> bool {
    let start = interval_raw((interval.0, interval.0));
    let interval = interval_raw(interval);
    row.exdates
        .iter()
        .any(|excluded| interval_raw(*excluded) == interval)
        || row.overridden.contains(&start)
}

fn overlaps((start, end): EpochInterval, (window_start, window_end): EpochInterval) -> bool {
    !(end < window_start || start > window_end)
}

/// RFC 5545 lets UNTIL be a DATE or a local datetime when DTSTART is; rrule
/// 0.14 requires UTC beside a zoned DTSTART. Rewrite such an UNTIL to the UTC
/// instant it names in `zone`; a DATE means the end of that local day, since
/// UNTIL is inclusive.
fn utc_until(rule: &str, zone: chrono_tz::Tz) -> String {
    rule.split(';')
        .map(|part| {
            let Some(value) = part
                .strip_prefix("UNTIL=")
                .filter(|value| !value.ends_with('Z'))
            else {
                return part.to_owned();
            };
            let local = NaiveDate::parse_from_str(value, "%Y%m%d")
                .ok()
                .and_then(|date| date.and_hms_opt(23, 59, 59))
                .or_else(|| NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%S").ok());
            match local {
                Some(local) => format!(
                    "UNTIL={}",
                    resolve_local(zone, local).format("%Y%m%dT%H%M%SZ")
                ),
                None => part.to_owned(),
            }
        })
        .collect::<Vec<_>>()
        .join(";")
}

fn recurrence_set(row: &OccurrenceEvent) -> Option<RRuleSet> {
    let rule = row.rrule.as_ref()?;
    let (base_start, base_end) = row.time;
    let duration = base_end - base_start;
    // Expanding from a zoned DTSTART keeps the local wall time across DST.
    // The start comes from the stored UTC instant, so it is never ambiguous.
    let zone = row.zone.unwrap_or(chrono_tz::UTC);
    let dtstart = epoch_to_chrono_utc(base_start)
        .ok()?
        .with_timezone(&Tz::Tz(zone));
    let mut set = utc_until(rule, zone)
        .parse::<RRule<Unvalidated>>()
        .ok()?
        .build(dtstart)
        .ok()?;

    // Let `all(1)` step past exact exclusions for `next`. Recurrence DTSTARTs
    // are serialized at whole-second precision above, so only equivalent
    // whole-second intervals are safe to hand to rrule's timestamp-based
    // EXDATE comparison. The exact interval check remains below as authority.
    for &(start, end) in &row.exdates {
        let start_utc = epoch_to_chrono_utc(start).ok()?;
        if end - start == duration && start_utc.timestamp_subsec_nanos() == 0 {
            set = set.exdate(start_utc.with_timezone(&Tz::UTC));
        }
    }
    Some(set)
}

fn rrule_occurrences(row: &OccurrenceEvent, window: EpochInterval) -> Option<Vec<EpochInterval>> {
    let (base_start, base_end) = row.time;
    let duration = base_end - base_start;
    let (window_start, window_end) = window;
    let mut occurrences = Vec::new();

    if row.rrule.is_some() {
        let result = recurrence_set(row)?
            .after(
                epoch_to_chrono_utc(window_start)
                    .ok()?
                    .with_timezone(&Tz::UTC),
            )
            .before(
                epoch_to_chrono_utc(window_end)
                    .ok()?
                    .with_timezone(&Tz::UTC),
            )
            .all(10_000);
        occurrences.extend(result.dates.into_iter().map(|datetime| {
            let start = chrono_to_epoch(datetime.with_timezone(&Utc));
            (start, start + duration)
        }));
    } else {
        occurrences.push((base_start, base_end));
    }
    occurrences.extend(row.rdates.iter().copied());
    occurrences.retain(|interval| !is_excluded(row, *interval) && overlaps(*interval, window));
    occurrences.sort_by_key(|interval| interval_raw(*interval));
    occurrences.dedup_by_key(|interval| interval_raw(*interval));
    Some(occurrences)
}

fn make_occurrence(
    row: &OccurrenceEvent,
    cancelled: bool,
    (start, end): EpochInterval,
) -> Occurrence {
    Occurrence {
        event_id: row.id,
        start,
        end,
        summary: row.summary.clone(),
        status: if cancelled {
            STATUS_CANCELLED.to_owned()
        } else {
            row.status.clone()
        },
        location: row.location.clone(),
    }
}

fn occurrence_key(occurrence: &Occurrence) -> ([u8; 32], Id) {
    (
        interval_raw((occurrence.start, occurrence.end)),
        occurrence.event_id,
    )
}

fn collect_occurrences<R, P>(
    reader: &R,
    space: &P,
    window: EpochInterval,
    show_cancelled: bool,
) -> Vec<Occurrence>
where
    R: BlobStoreGet,
    P: TriblePattern + ?Sized,
{
    let mut occurrences = Vec::new();
    for event_id in events_model::current_event_ids(space) {
        let Some(row) = occurrence_event(reader, space, event_id) else {
            continue;
        };
        let cancelled = events_model::event_is_cancelled(space, row.id);
        if cancelled && !show_cancelled {
            continue;
        }
        let Some(event_occurrences) = rrule_occurrences(&row, window) else {
            continue;
        };
        for interval in event_occurrences {
            occurrences.push(make_occurrence(&row, cancelled, interval));
        }
    }
    occurrences.sort_by_key(occurrence_key);
    occurrences
}

fn next_event_occurrence(row: &OccurrenceEvent, window: EpochInterval) -> Option<EpochInterval> {
    let (window_start, window_end) = window;
    let mut next = if row.rrule.is_some() {
        let (base_start, base_end) = row.time;
        let duration = base_end - base_start;
        recurrence_set(row)?
            .after(
                epoch_to_chrono_utc(window_start)
                    .ok()?
                    .with_timezone(&Tz::UTC),
            )
            .before(
                epoch_to_chrono_utc(window_end)
                    .ok()?
                    .with_timezone(&Tz::UTC),
            )
            .all(1)
            .dates
            .into_iter()
            .next()
            .map(|datetime| {
                let start = chrono_to_epoch(datetime.with_timezone(&Utc));
                (start, start + duration)
            })
    } else {
        Some(row.time)
    }
    .filter(|interval| !is_excluded(row, *interval) && overlaps(*interval, window));

    for &rdate in &row.rdates {
        if is_excluded(row, rdate) || !overlaps(rdate, window) {
            continue;
        }
        if next
            .as_ref()
            .is_none_or(|current| interval_raw(rdate) < interval_raw(*current))
        {
            next = Some(rdate);
        }
    }
    next
}

fn next_occurrence<R, P>(reader: &R, space: &P, window: EpochInterval) -> Option<Occurrence>
where
    R: BlobStoreGet,
    P: TriblePattern + ?Sized,
{
    let mut next = None;
    for event_id in events_model::current_event_ids(space) {
        let Some(row) = occurrence_event(reader, space, event_id) else {
            continue;
        };
        if events_model::event_is_cancelled(space, row.id) {
            continue;
        }
        let Some(interval) = next_event_occurrence(&row, window) else {
            continue;
        };
        let candidate = make_occurrence(&row, false, interval);
        if next
            .as_ref()
            .is_none_or(|current| occurrence_key(&candidate) < occurrence_key(current))
        {
            next = Some(candidate);
        }
    }
    next
}

pub fn local_day_window(days: i64) -> Result<(Epoch, Epoch)> {
    let timezone = chrono::Local;
    let now = epoch_to_chrono_utc(clock::now()?)?.with_timezone(&timezone);
    let start = now
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .expect("midnight exists");
    let end = start + chrono::Duration::days(days);
    let start = timezone
        .from_local_datetime(&start)
        .single()
        .ok_or_else(|| anyhow!("local start-of-day is ambiguous or unavailable"))?;
    let end = timezone
        .from_local_datetime(&end)
        .single()
        .ok_or_else(|| anyhow!("local end-of-day is ambiguous or unavailable"))?;
    Ok((
        chrono_to_epoch(start.with_timezone(&Utc)),
        chrono_to_epoch(end.with_timezone(&Utc)),
    ))
}

#[derive(Debug)]
struct IcalEvent {
    uid: String,
    summary: Option<String>,
    description: Option<String>,
    dtstart: DateTime<Utc>,
    dtend: DateTime<Utc>,
    /// Zone of DTSTART, recorded so an RRULE expands in local time.
    zone: Option<chrono_tz::Tz>,
    location: Option<String>,
    rrule: Option<String>,
    exdates: BTreeSet<IntervalValue>,
    recurrence_id: Option<IntervalValue>,
    status: Option<String>,
    transp: Option<String>,
    sequence: Option<u64>,
    last_modified: Option<DateTime<Utc>>,
    /// When the source wrote this VEVENT out. Not stored on the event (in an
    /// export it is the export time); it dates a complete export.
    dtstamp: Option<DateTime<Utc>>,
}

/// TZIDs a calendar defines itself. Only a VTIMEZONE whose every offset is
/// the same whole number of hours is understood: it is the fixed IANA zone
/// `Etc/GMT∓N` (POSIX sign: `Etc/GMT+7` is UTC-07:00). An IANA TZID always
/// resolves through chrono-tz first, whatever its VTIMEZONE says.
type ZoneTable = BTreeMap<String, chrono_tz::Tz>;

fn calendar_zones(calendar: &ical::parser::ical::component::IcalCalendar) -> ZoneTable {
    let mut zones = ZoneTable::new();
    for timezone in &calendar.timezones {
        let Some(name) = timezone
            .properties
            .iter()
            .find(|property| property.name == "TZID")
            .and_then(|property| property.value.clone())
        else {
            continue;
        };
        let offsets = timezone
            .transitions
            .iter()
            .flat_map(|transition| transition.properties.iter())
            .filter(|property| matches!(property.name.as_str(), "TZOFFSETFROM" | "TZOFFSETTO"))
            .map(|property| property.value.as_deref().and_then(parse_utc_offset))
            .collect::<Option<BTreeSet<i32>>>();
        let Some(offsets) = offsets else { continue };
        let [seconds] = offsets.into_iter().collect::<Vec<_>>()[..] else {
            continue;
        };
        if seconds % 3600 != 0 {
            continue;
        }
        let hours = -seconds / 3600;
        let iana = match hours {
            0 => "Etc/GMT".to_owned(),
            hours => format!("Etc/GMT{hours:+}"),
        };
        if let Ok(zone) = iana.parse::<chrono_tz::Tz>() {
            zones.insert(name, zone);
        }
    }
    zones
}

/// `-0700`, `+0530` or `+013000` as seconds east of UTC.
fn parse_utc_offset(value: &str) -> Option<i32> {
    let value = value.trim();
    let (sign, digits) = match value.as_bytes().first()? {
        b'+' => (1, &value[1..]),
        b'-' => (-1, &value[1..]),
        _ => return None,
    };
    if !matches!(digits.len(), 4 | 6) || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let hours: i32 = digits[0..2].parse().ok()?;
    let minutes: i32 = digits[2..4].parse().ok()?;
    let seconds: i32 = digits.get(4..6).map_or(Some(0), |s| s.parse().ok())?;
    Some(sign * (hours * 3600 + minutes * 60 + seconds))
}

fn resolve_zone(name: &str, zones: &ZoneTable) -> Result<chrono_tz::Tz> {
    name.parse::<chrono_tz::Tz>()
        .ok()
        .or_else(|| zones.get(name).copied())
        .ok_or_else(|| {
            anyhow!(
                "unknown TZID '{name}': not an IANA zone and no fixed-offset VTIMEZONE defines it"
            )
        })
}

/// A local time in `zone` as the instant it names. RFC 5545 §3.3.5: a local
/// time that does not exist (spring-forward gap) is read with the offset in
/// force before the gap, so it moves forward by the gap's length (02:30 in
/// Berlin on the last Sunday of March is 03:30 CEST); an ambiguous one
/// (fall-back overlap) is the first occurrence, the earlier instant.
pub(super) fn resolve_local(zone: chrono_tz::Tz, local: NaiveDateTime) -> DateTime<Utc> {
    use chrono::offset::{LocalResult, Offset};
    match zone.from_local_datetime(&local) {
        LocalResult::Single(datetime) => datetime.with_timezone(&Utc),
        LocalResult::Ambiguous(first, second) => first
            .min(second.with_timezone(&first.timezone()))
            .with_timezone(&Utc),
        LocalResult::None => {
            // No real zone has two transitions within a day.
            let before = zone
                .offset_from_utc_datetime(&(local - chrono::Duration::days(1)))
                .fix()
                .local_minus_utc();
            Utc.from_utc_datetime(&(local - chrono::Duration::seconds(before.into())))
        }
    }
}

fn property_param<'a>(property: &'a ical::property::Property, name: &str) -> Option<&'a str> {
    property.params.as_ref()?.iter().find_map(|(key, values)| {
        (key.eq_ignore_ascii_case(name))
            .then(|| values.first().map(String::as_str))
            .flatten()
    })
}

/// One DATE or DATE-TIME value: its instant, whether it is a whole date, and
/// the zone it was written in (`None` for UTC and for dates).
struct IcalTime {
    instant: DateTime<Utc>,
    is_date: bool,
    zone: Option<chrono_tz::Tz>,
}

fn parse_ical_time(
    value: &str,
    property: &ical::property::Property,
    zones: &ZoneTable,
    options: &IngestOptions,
) -> Result<IcalTime> {
    let value = value.trim();
    let name = &property.name;
    if property_param(property, "VALUE") == Some("DATE") || value.len() == 8 {
        // A DATE stays an all-day date at UTC midnight, the stored convention.
        let date = NaiveDate::parse_from_str(value, "%Y%m%d")
            .with_context(|| format!("parse {name} date '{value}'"))?;
        return Ok(IcalTime {
            instant: Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0).expect("midnight exists")),
            is_date: true,
            zone: None,
        });
    }
    if let Some(utc) = value.strip_suffix('Z') {
        let datetime = NaiveDateTime::parse_from_str(utc, "%Y%m%dT%H%M%S")
            .with_context(|| format!("parse {name} UTC datetime '{value}'"))?;
        return Ok(IcalTime {
            instant: Utc.from_utc_datetime(&datetime),
            is_date: false,
            zone: None,
        });
    }
    let local = NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%S")
        .with_context(|| format!("parse {name} datetime '{value}'"))?;
    let zone = match property_param(property, "TZID") {
        Some(tzid) => resolve_zone(tzid, zones).with_context(|| format!("{name}"))?,
        None => options.floating_zone.ok_or_else(|| {
            anyhow!(
                "{name} '{value}' is a floating time (no Z, no TZID); \
                 supply a floating zone to say which local time it is"
            )
        })?,
    };
    Ok(IcalTime {
        instant: resolve_local(zone, local),
        is_date: false,
        zone: Some(zone),
    })
}

fn set_once(slot: &mut Option<String>, field: &str, value: String) -> Result<()> {
    if slot.is_some() {
        bail!("VEVENT has more than one {field} property");
    }
    *slot = Some(value);
    Ok(())
}

fn parse_ical_event(
    event: &ical::parser::ical::component::IcalEvent,
    zones: &ZoneTable,
    options: &IngestOptions,
) -> Result<IcalEvent> {
    let mut uid = None;
    let mut summary = None;
    let mut description = None;
    let mut dtstart = None;
    let mut dtend = None;
    let mut location = None;
    let mut rrule = None;
    let mut status = None;
    let mut transp = None;
    let mut recurrence_id = None;
    let mut exdates = Vec::new();
    let mut sequence = None;
    let mut last_modified = None;
    let mut dtstamp = None;

    for property in &event.properties {
        let value = property.value.clone().unwrap_or_default();
        match property.name.as_str() {
            "UID" => set_once(&mut uid, "UID", value)?,
            "SUMMARY" => set_once(&mut summary, "SUMMARY", value)?,
            "DESCRIPTION" => set_once(&mut description, "DESCRIPTION", value)?,
            "DTSTART" => {
                if dtstart.replace(property).is_some() {
                    bail!("VEVENT has more than one DTSTART property");
                }
            }
            "DTEND" => {
                if dtend.replace(property).is_some() {
                    bail!("VEVENT has more than one DTEND property");
                }
            }
            "RECURRENCE-ID" => {
                if recurrence_id.replace(property).is_some() {
                    bail!("VEVENT has more than one RECURRENCE-ID property");
                }
            }
            "EXDATE" => exdates.push(property),
            "LOCATION" => set_once(&mut location, "LOCATION", value)?,
            "RRULE" => set_once(&mut rrule, "RRULE", value)?,
            "STATUS" => set_once(&mut status, "STATUS", value)?,
            "TRANSP" => set_once(&mut transp, "TRANSP", value)?,
            "SEQUENCE" => set_once(&mut sequence, "SEQUENCE", value)?,
            "LAST-MODIFIED" => {
                if last_modified.replace(property).is_some() {
                    bail!("VEVENT has more than one LAST-MODIFIED property");
                }
            }
            "DTSTAMP" => {
                if dtstamp.replace(property).is_some() {
                    bail!("VEVENT has more than one DTSTAMP property");
                }
            }
            _ => {}
        }
    }

    let uid = uid.ok_or_else(|| anyhow!("VEVENT missing UID"))?;
    let named = |what: &str| format!("{what} of VEVENT UID '{uid}'");
    let time = |property: &ical::property::Property| {
        parse_ical_time(
            property.value.as_deref().unwrap_or_default(),
            property,
            zones,
            options,
        )
    };
    let start_property = dtstart.ok_or_else(|| anyhow!("VEVENT UID '{uid}' missing DTSTART"))?;
    let start = time(start_property).with_context(|| named("DTSTART"))?;
    let dtend = match dtend {
        Some(property) => time(property).with_context(|| named("DTEND"))?.instant,
        None if start.is_date => start.instant + chrono::Duration::days(1),
        None => start.instant + chrono::Duration::hours(1),
    };
    if dtend < start.instant {
        bail!("VEVENT UID '{uid}' DTEND is before DTSTART");
    }
    let duration = dtend - start.instant;
    let mut excluded = BTreeSet::new();
    for property in exdates {
        // One EXDATE line may list several comma-separated values.
        for value in property.value.as_deref().unwrap_or_default().split(',') {
            let excluded_start = parse_ical_time(value, property, zones, options)
                .with_context(|| named("EXDATE"))?
                .instant;
            excluded.insert(make_interval(
                chrono_to_epoch(excluded_start),
                chrono_to_epoch(excluded_start + duration),
            ));
        }
    }
    let recurrence_id = recurrence_id
        .map(|property| -> Result<IntervalValue> {
            let instant = chrono_to_epoch(
                time(property)
                    .with_context(|| named("RECURRENCE-ID"))?
                    .instant,
            );
            Ok(make_interval(instant, instant))
        })
        .transpose()?;

    let sequence = sequence
        .map(|value| {
            value
                .trim()
                .parse::<u64>()
                .with_context(|| named("SEQUENCE"))
        })
        .transpose()?;
    let last_modified = last_modified
        .map(|property| time(property).with_context(|| named("LAST-MODIFIED")))
        .transpose()?
        .map(|value| value.instant);
    let dtstamp = dtstamp
        .map(|property| time(property).with_context(|| named("DTSTAMP")))
        .transpose()?
        .map(|value| value.instant);

    Ok(IcalEvent {
        uid,
        summary,
        description,
        dtstart: start.instant,
        dtend,
        zone: start.zone,
        location,
        rrule,
        exdates: excluded,
        recurrence_id,
        status,
        transp,
        sequence,
        last_modified,
        dtstamp,
    })
}

fn single_line(value: &str) -> String {
    value.replace('\n', " ")
}

/// What staging one record did.
enum Staged {
    /// The same definition is already resident or staged.
    Duplicate,
    /// A new lineage.
    New,
    /// A new version of a lineage already resident or staged.
    Revision,
}

/// Stage one record into a deterministic map keyed by its content-derived id.
/// Nothing is refused: an unchanged definition is a duplicate, a changed one
/// a new version, whatever the order of files and VEVENTs.
fn stage_import_event(
    loaded: &LoadedEvents,
    staged: &mut BTreeMap<Id, Fragment>,
    rereadings: &mut Vec<Fragment>,
    fragment: Fragment,
) -> Result<Staged> {
    let id = fragment
        .root()
        .ok_or_else(|| anyhow!("constructed iCalendar event has no root"))?;
    let candidate = events_model::event(fragment.facts(), id)
        .ok_or_else(|| anyhow!("constructed iCalendar event has no readable projection"))?;
    let resident = events_model::lineage_versions(&loaded.facts, &candidate);
    if staged.contains_key(&id)
        || resident
            .iter()
            .any(|existing| same_event_definition(existing, &candidate))
    {
        return Ok(Staged::Duplicate);
    }
    let lineage = events_model::lineage(&candidate);
    let known = !resident.is_empty()
        || staged.values().any(|other| {
            other
                .root()
                .and_then(|root| events_model::event(other.facts(), root))
                .is_some_and(|row| events_model::lineage(&row) == lineage)
        });
    // A resident version of the same source revision that reads differently
    // was read by an earlier ingest; this reading replaces it.
    for existing in &resident {
        if existing.sequence == candidate.sequence
            && existing.last_modified == candidate.last_modified
        {
            rereadings.push(rereading_fragment(existing.id, id));
        }
    }
    staged.insert(id, fragment);
    Ok(if known { Staged::Revision } else { Staged::New })
}

fn same_event_definition(left: &EventRow, right: &EventRow) -> bool {
    left.uid == right.uid
        && left.summary == right.summary
        && left.description == right.description
        && left.time == right.time
        && left.rrule == right.rrule
        && left.rdates == right.rdates
        && left.exdates == right.exdates
        && left.location == right.location
        && left.status == right.status
        && left.transp == right.transp
        && left.attendees == right.attendees
        && left.organizer == right.organizer
        && left.sequence == right.sequence
        && left.tzid == right.tzid
        && left.recurrence_id == right.recurrence_id
        && left.last_modified == right.last_modified
}

fn ingest(
    storage: EventsStorage<'_>,
    documents: &[CalendarInput<'_>],
    options: IngestOptions,
) -> Result<IngestReceipt> {
    validate_calendars(documents)?;
    if let Some(calendar) = &options.complete_calendar {
        events_model::validate_label("export calendar", calendar)?;
        if documents.len() != 1 {
            bail!(
                "a complete export is one document; {} were supplied",
                documents.len()
            );
        }
    }
    storage.update("ingest iCalendar events", |loaded| {
        let mut staged = BTreeMap::<Id, Fragment>::new();
        let mut rereadings = Vec::new();
        let mut total = 0usize;
        let mut duplicates = 0usize;
        let mut revisions = 0usize;
        // Every version the document holds, staged or already resident, and
        // the latest DTSTAMP: what a complete export observation records.
        let mut held = BTreeSet::<Id>::new();
        let mut held_lineages = BTreeSet::<events_model::Lineage>::new();
        let mut exported_at = None::<DateTime<Utc>>;

        for document in documents {
            for calendar in ical::IcalParser::new(document.text.as_bytes()) {
                let calendar = calendar.with_context(|| format!("parse {}", document.name))?;
                let zones = calendar_zones(&calendar);
                for source in &calendar.events {
                    total += 1;
                    let source = parse_ical_event(source, &zones, &options)
                        .with_context(|| format!("parse VEVENT in {}", document.name))?;
                    exported_at = exported_at.max(source.dtstamp);
                    let mut draft = empty_event_draft(
                        source.uid.clone(),
                        single_line(
                            &source
                                .summary
                                .as_deref()
                                .map(unescape_text)
                                .unwrap_or_else(|| "(untitled)".to_owned()),
                        ),
                        make_interval(
                            chrono_to_epoch(source.dtstart),
                            chrono_to_epoch(source.dtend),
                        ),
                        normalized_status(source.status.as_deref()),
                        normalized_transp(source.transp.as_deref()),
                    );
                    draft.description = source.description.as_deref().map(unescape_text);
                    draft.location = source
                        .location
                        .as_deref()
                        .map(|location| single_line(&unescape_text(location)));
                    draft.rrule = source.rrule;
                    draft.exdates = source.exdates;
                    draft.recurrence_id = source.recurrence_id;
                    draft.tzid = source.zone.map(|zone| zone.name().to_owned());
                    draft.sequence = source.sequence.map(|value| value.to_inline());
                    draft.last_modified = source.last_modified.map(|instant| {
                        let instant = chrono_to_epoch(instant);
                        make_interval(instant, instant)
                    });
                    let fragment = event_fragment(&draft)?;
                    let id = fragment.root().expect("event fragment has one root");
                    held.insert(id);
                    if let Some(row) = events_model::event(fragment.facts(), id) {
                        held_lineages.insert(events_model::lineage(&row));
                    }
                    match stage_import_event(loaded, &mut staged, &mut rereadings, fragment)? {
                        Staged::Duplicate => duplicates += 1,
                        Staged::Revision => revisions += 1,
                        Staged::New => {}
                    }
                }
            }
        }

        let mut export = None;
        let mut export_fragment = None;
        if let Some(calendar) = &options.complete_calendar {
            let exported_at = exported_at.ok_or_else(|| {
                anyhow!(
                    "a complete export is dated by its latest DTSTAMP, and {} has none",
                    documents[0].name
                )
            })?;
            let instant = chrono_to_epoch(exported_at);
            let observation =
                events_model::export_fragment(calendar, make_interval(instant, instant), &held)?;
            let id = observation.root().expect("export fragment has one root");
            let recorded = !exists!(pattern!(&loaded.facts, [{ id @
                metadata::tag: &KIND_EXPORT_ID,
            }]));
            let handle = events_model::text_handle(calendar);
            let earlier = events_model::exports(&loaded.facts)
                .into_iter()
                .filter(|row| row.calendar == handle)
                .collect::<Vec<_>>();
            let point = make_interval(instant, instant).raw;
            let newest = earlier.iter().all(|row| row.exported_at.raw <= point);
            let absent = earlier
                .iter()
                .flat_map(|row| row.versions.iter())
                .filter_map(|version| events_model::event(&loaded.facts, *version))
                .map(|row| events_model::lineage(&row))
                .filter(|lineage| !held_lineages.contains(lineage))
                .collect::<BTreeSet<_>>()
                .len();
            export = Some(ExportReceipt {
                calendar: calendar.clone(),
                exported_at: instant,
                recorded,
                newest,
                absent,
            });
            export_fragment = recorded.then_some(observation);
        }

        let imported = staged
            .values()
            .map(|fragment| fragment.root().expect("event fragment has one root"))
            .collect::<Vec<_>>();
        let fragment = if imported.is_empty() && export_fragment.is_none() {
            None
        } else {
            let mut fragment = Fragment::empty();
            for event in staged.into_values() {
                fragment += event;
            }
            for rereading in rereadings {
                fragment += rereading;
            }
            if let Some(observation) = export_fragment {
                fragment += observation;
            }
            Some(fragment)
        };
        Ok((
            fragment,
            IngestReceipt {
                imported,
                total,
                duplicates,
                revisions,
                export,
            },
        ))
    })
}

#[cfg(test)]
#[test]
fn event_and_initial_note_are_one_commit_a_preparing_reader_observes() {
    let directory = tempfile::tempdir().unwrap();
    let pile = directory.path().join("planner.pile");
    let key = directory.path().join("planner.key");
    std::fs::File::create(&pile).unwrap();
    crate::storage::initialize_signer(&pile, Some(&key)).unwrap();
    let capability = Events::new(pile, Some(key));
    let mut options = AddOptions::new(
        "Already visible",
        (
            Epoch::from_unix_seconds(10.0),
            Epoch::from_unix_seconds(20.0),
        ),
    );
    options.note = Some("Part of the same COMMIT".to_owned());
    let added = capability.add(&options).unwrap();
    capability
        .storage
        .with_pile(|pile, signer| {
            let source = open_configured(pile, DEFAULT_SCOPE_ID, signer.verifying_key())?;
            let succinct = pile.attach::<SuccinctArchiveBlob>(source, ())?;
            let rank9 = pile.attach::<Rank9AcceleratedSuccinctArchiveBlob>(source, succinct)?;
            let snapshot = pollster::block_on(async {
                drop(pile.maintain_attached(succinct, signer).await?);
                pile.maintain_attached(rank9, signer).await
            })?;
            let selected = snapshot.attached(rank9)?;
            let facts = selected.facts()?;
            assert!(events_model::event(&facts, added.event).is_some());
            assert_eq!(events_model::notes_for_event(&facts, added.event).len(), 1);
            assert_eq!(
                source.admitted(&snapshot)?.len(),
                1,
                "the action remains one COMMIT"
            );
            Ok(())
        })
        .unwrap();
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
