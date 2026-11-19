//! Canonical Planner records and open-world collection projections.
//!
//! An event is an immutable VEVENT-shaped record, and its identity core is
//! the whole record: every stored field, the UID included. One VEVENT
//! definition is one entity, so re-importing the same definition is a
//! duplicate, and a changed definition (an edit, a move, a new EXDATE) is a
//! new observation beside the old one, never a conflict. The records that
//! share a UID and RECURRENCE-ID are the versions of one lineage; a reader
//! takes the current version by SEQUENCE, then LAST-MODIFIED, then a
//! rereading (a later ingest's reading of the same revision replaces an
//! earlier one), then id ([`current_event_ids`]). Local cancellation is a separate intrinsic
//! assertion about one version that readers apply to its whole lineage.
//! This keeps cancellation monotone: set union can make an event cancelled,
//! but can never accidentally resurrect it by choosing one of several scalar
//! `status` values.
//!
//! Ordinary readers query the maintained fact archive directly and skip rows
//! they cannot decode.

use std::collections::{BTreeMap, BTreeSet};

pub mod cli;
pub mod mcp;
mod operations;
pub mod presentation;
pub use operations::*;

use anyhow::{anyhow, bail, Context, Result};
use triblespace::core::metadata;
use triblespace::core::query::TriblePattern;
use triblespace::core::repo::BlobStoreGet;
use triblespace::prelude::*;

use crate::schemas::planner::{
    cancellation, event, note, rereading, KIND_CANCELLATION_ID, KIND_EVENT_ID, KIND_NOTE_ID,
    KIND_REREADING_ID,
};

pub type IntervalValue = Inline<inlineencodings::NsTAIInterval>;
pub type TextHandle = Inline<inlineencodings::Handle<blobencodings::UTF8String>>;
pub type SequenceValue = Inline<inlineencodings::U256BE>;

pub const STATUS_CONFIRMED: &str = "CONFIRMED";
pub const STATUS_TENTATIVE: &str = "TENTATIVE";
pub const STATUS_CANCELLED: &str = "CANCELLED";
pub const TRANSP_OPAQUE: &str = "OPAQUE";
pub const TRANSP_TRANSPARENT: &str = "TRANSPARENT";

/// Complete immutable input for one canonical Planner event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EventDraft {
    pub uid: String,
    pub summary: String,
    pub description: Option<String>,
    pub time: IntervalValue,
    pub rrule: Option<String>,
    pub rdates: BTreeSet<IntervalValue>,
    pub exdates: BTreeSet<IntervalValue>,
    pub location: Option<String>,
    pub status: String,
    pub transp: String,
    pub attendees: BTreeSet<Id>,
    pub organizer: Option<Id>,
    pub sequence: Option<SequenceValue>,
    /// IANA zone of the source DTSTART, read when an RRULE is expanded.
    pub tzid: Option<String>,
    /// RFC 5545 RECURRENCE-ID: original start of the overridden instance.
    pub recurrence_id: Option<IntervalValue>,
    /// RFC 5545 LAST-MODIFIED, a point interval.
    pub last_modified: Option<IntervalValue>,
}

/// One exact event projected from a Planner collection. Text fields are the
/// handles of their UTF-8 blobs; equal handles are equal text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EventRow {
    pub id: Id,
    pub uid: TextHandle,
    pub summary: TextHandle,
    pub description: Option<TextHandle>,
    pub time: IntervalValue,
    pub rrule: Option<TextHandle>,
    pub rdates: BTreeSet<IntervalValue>,
    pub exdates: BTreeSet<IntervalValue>,
    pub location: Option<TextHandle>,
    pub status: TextHandle,
    pub transp: TextHandle,
    pub attendees: BTreeSet<Id>,
    pub organizer: Option<Id>,
    pub sequence: Option<SequenceValue>,
    pub tzid: Option<TextHandle>,
    pub recurrence_id: Option<IntervalValue>,
    pub last_modified: Option<IntervalValue>,
}

/// One intrinsic note attached to an event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NoteRow {
    pub id: Id,
    pub event: Id,
    pub text: TextHandle,
    pub created_at: IntervalValue,
}

fn validate_label(field: &str, value: &str) -> Result<()> {
    if value.as_bytes().contains(&0) {
        bail!("{field} contains a NUL byte");
    }
    Ok(())
}

fn validate_uid(uid: &str) -> Result<()> {
    if uid.is_empty() {
        bail!("iCalendar UID is empty");
    }
    if uid.bytes().any(|byte| matches!(byte, 0 | b'\r' | b'\n')) {
        bail!("iCalendar UID contains a forbidden control character");
    }
    Ok(())
}

fn validate_interval(field: &str, interval: IntervalValue) -> Result<()> {
    let (start, end): (i128, i128) = interval
        .try_from_inline()
        .map_err(|error| anyhow!("decode {field}: {error:?}"))?;
    if end < start {
        bail!("{field} ends before it starts");
    }
    Ok(())
}

fn validate_point(field: &str, interval: IntervalValue) -> Result<()> {
    let (start, end): (i128, i128) = interval
        .try_from_inline()
        .map_err(|error| anyhow!("decode {field}: {error:?}"))?;
    if start != end {
        bail!("{field} must be a point interval");
    }
    Ok(())
}

fn validate_status(status: &str) -> Result<()> {
    if !matches!(
        status,
        STATUS_CONFIRMED | STATUS_TENTATIVE | STATUS_CANCELLED
    ) {
        bail!("event status must be CONFIRMED, TENTATIVE, or CANCELLED");
    }
    Ok(())
}

fn validate_transp(transp: &str) -> Result<()> {
    if !matches!(transp, TRANSP_OPAQUE | TRANSP_TRANSPARENT) {
        bail!("event transparency must be OPAQUE or TRANSPARENT");
    }
    Ok(())
}

/// The content handle of `value` as text. Closed words (status,
/// transparency) are compared by it without reading the stored blob.
pub fn text_handle(value: &str) -> TextHandle {
    value.to_owned().to_blob().get_handle()
}

fn validate_draft_values(draft: &EventDraft) -> Result<()> {
    validate_label("event summary", &draft.summary)?;
    if let Some(rule) = &draft.rrule {
        validate_label("event RRULE", rule)?;
    }
    if let Some(location) = &draft.location {
        validate_label("event location", location)?;
    }
    validate_label("event status", &draft.status)?;
    validate_status(&draft.status)?;
    validate_label("event transparency", &draft.transp)?;
    validate_transp(&draft.transp)?;
    if let Some(zone) = &draft.tzid {
        zone.parse::<chrono_tz::Tz>()
            .map_err(|_| anyhow!("event TZID '{zone}' is not an IANA time zone"))?;
    }
    if let Some(recurrence_id) = draft.recurrence_id {
        validate_point("event RECURRENCE-ID", recurrence_id)?;
    }
    Ok(())
}

fn validate_event_values(row: &EventRow) -> Result<()> {
    validate_interval("event time", row.time)?;
    for interval in &row.rdates {
        validate_interval("event RDATE", *interval)?;
    }
    for interval in &row.exdates {
        validate_interval("event EXDATE", *interval)?;
    }
    if ![STATUS_CONFIRMED, STATUS_TENTATIVE, STATUS_CANCELLED]
        .into_iter()
        .any(|status| text_handle(status) == row.status)
    {
        bail!("event status must be CONFIRMED, TENTATIVE, or CANCELLED");
    }
    if ![TRANSP_OPAQUE, TRANSP_TRANSPARENT]
        .into_iter()
        .any(|transp| text_handle(transp) == row.transp)
    {
        bail!("event transparency must be OPAQUE or TRANSPARENT");
    }
    Ok(())
}

/// One event record. Its id is intrinsic over every field, so equal
/// definitions are one entity and any change is a different entity.
#[allow(clippy::too_many_arguments)]
fn event_record(
    uid: TextHandle,
    summary: TextHandle,
    description: Option<TextHandle>,
    time: IntervalValue,
    rrule: Option<TextHandle>,
    rdates: &BTreeSet<IntervalValue>,
    exdates: &BTreeSet<IntervalValue>,
    location: Option<TextHandle>,
    status: TextHandle,
    transp: TextHandle,
    attendees: &BTreeSet<Id>,
    organizer: Option<Id>,
    sequence: Option<SequenceValue>,
    tzid: Option<TextHandle>,
    recurrence_id: Option<IntervalValue>,
    last_modified: Option<IntervalValue>,
) -> Fragment {
    entity! {
        metadata::tag: &KIND_EVENT_ID,
        event::ical_uid: uid,
        event::summary: summary,
        event::description?: description.as_ref(),
        event::time: time,
        event::rrule?: rrule.as_ref(),
        event::rdate*: rdates.iter(),
        event::exdate*: exdates.iter(),
        event::location?: location.as_ref(),
        event::status: status,
        event::transp: transp,
        event::attendee*: attendees.iter(),
        event::organizer?: organizer.as_ref(),
        event::sequence?: sequence.as_ref(),
        event::tzid?: tzid.as_ref(),
        event::recurrence_id?: recurrence_id.as_ref(),
        event::last_modified?: last_modified.as_ref(),
    }
}

fn note_record(event_id: Id, text: TextHandle, created_at: IntervalValue) -> Fragment {
    entity! {
        metadata::tag: &KIND_NOTE_ID,
        metadata::created_at: created_at,
        note::note_about: event_id,
        note::note_text: text,
    }
}

fn cancellation_record(event_id: Id) -> Fragment {
    entity! {
        metadata::tag: &KIND_CANCELLATION_ID,
        cancellation::event: event_id,
    }
}

/// Build one self-contained canonical event fragment.
///
/// The returned root depends only on the definition. Repeating an import of
/// an unchanged event therefore names the same entity even across processes
/// and machines; an edited event gets a new id.
pub fn event_fragment(draft: &EventDraft) -> Result<Fragment> {
    validate_uid(&draft.uid)?;
    validate_draft_values(draft)?;
    let mut fragment = Fragment::empty();
    let uid = fragment.put(draft.uid.clone());
    let summary = fragment.put(draft.summary.clone());
    let description = draft
        .description
        .as_ref()
        .map(|description| fragment.put(description.clone()));
    let rrule = draft.rrule.as_ref().map(|rule| fragment.put(rule.clone()));
    let location = draft
        .location
        .as_ref()
        .map(|location| fragment.put(location.clone()));
    let status = fragment.put(draft.status.clone());
    let transp = fragment.put(draft.transp.clone());
    let tzid = draft.tzid.as_ref().map(|zone| fragment.put(zone.clone()));
    let record = event_record(
        uid,
        summary,
        description,
        draft.time,
        rrule,
        &draft.rdates,
        &draft.exdates,
        location,
        status,
        transp,
        &draft.attendees,
        draft.organizer,
        draft.sequence,
        tzid,
        draft.recurrence_id,
        draft.last_modified,
    );
    let row = EventRow {
        id: record.root().expect("event record has one root"),
        uid,
        summary,
        description,
        time: draft.time,
        rrule,
        rdates: draft.rdates.clone(),
        exdates: draft.exdates.clone(),
        location,
        status,
        transp,
        attendees: draft.attendees.clone(),
        organizer: draft.organizer,
        sequence: draft.sequence,
        tzid,
        recurrence_id: draft.recurrence_id,
        last_modified: draft.last_modified,
    };
    validate_event_values(&row)?;
    fragment += record;
    Ok(fragment)
}

/// Build one intrinsic note and carry its text attachment.
pub fn note_fragment(event_id: Id, text: &str, created_at: IntervalValue) -> Result<Fragment> {
    validate_point("note creation time", created_at)?;
    let mut fragment = Fragment::empty();
    let text = fragment.put(text.to_owned());
    fragment += note_record(event_id, text, created_at);
    Ok(fragment)
}

/// Build the assertion that `replacement` is a later reading of the same
/// source revision as `replaced`.
pub fn rereading_fragment(replaced: Id, replacement: Id) -> Fragment {
    entity! {
        metadata::tag: &KIND_REREADING_ID,
        rereading::replaced: replaced,
        rereading::replacement: replacement,
    }
}

/// Every version some rereading replaces. A cycle of rereadings replaces
/// all its members, which then fall back to the id tie-break.
pub fn reread_event_ids<P>(space: &P) -> BTreeSet<Id>
where
    P: TriblePattern + ?Sized,
{
    find!(
        replaced: Id,
        pattern!(space, [{ _?reading @
            metadata::tag: &KIND_REREADING_ID,
            rereading::replaced: ?replaced,
            rereading::replacement: _?replacement,
        }])
    )
    .collect()
}

/// Build the unique monotone cancellation assertion for an event.
pub fn cancellation_fragment(event_id: Id) -> Fragment {
    cancellation_record(event_id)
}

/// IDs of all events inhabiting the typed Planner projection.
pub fn event_ids<P>(space: &P) -> Vec<Id>
where
    P: TriblePattern + ?Sized,
{
    find!(
        id: Id,
        pattern!(space, [{ ?id @
            metadata::tag: &KIND_EVENT_ID,
            event::ical_uid: _?uid,
            event::summary: _?summary,
            event::time: _?time,
            event::status: _?status,
            event::transp: _?transp,
        }])
    )
    .collect()
}

/// Project one event if all fields needed by this reader are present.
///
/// Unknown facts and unrelated extensions remain invisible. Where this
/// version of the Planner model exposes a scalar field, the first value in
/// pattern order is its deterministic presentation choice.
pub fn event<P>(space: &P, id: Id) -> Option<EventRow>
where
    P: TriblePattern + ?Sized,
{
    find!(
        (
            uid: TextHandle,
            summary: TextHandle,
            time: IntervalValue,
            status: TextHandle,
            transp: TextHandle,
        ),
        pattern!(space, [{ id @
            metadata::tag: &KIND_EVENT_ID,
            event::ical_uid: ?uid,
            event::summary: ?summary,
            event::time: ?time,
            event::status: ?status,
            event::transp: ?transp,
        }])
    )
    .find_map(|(uid, summary, time, status, transp)| {
        let row = EventRow {
            id,
            uid,
            summary,
            description: find!(
                value: TextHandle,
                pattern!(space, [{ id @ event::description: ?value }])
            )
            .next(),
            time,
            rrule: find!(
                value: TextHandle,
                pattern!(space, [{ id @ event::rrule: ?value }])
            )
            .next(),
            rdates: find!(
                value: IntervalValue,
                pattern!(space, [{ id @ event::rdate: ?value }])
            )
            .collect(),
            exdates: find!(
                value: IntervalValue,
                pattern!(space, [{ id @ event::exdate: ?value }])
            )
            .collect(),
            location: find!(
                value: TextHandle,
                pattern!(space, [{ id @ event::location: ?value }])
            )
            .next(),
            status,
            transp,
            attendees: find!(
                value: Id,
                pattern!(space, [{ id @ event::attendee: ?value }])
            )
            .collect(),
            organizer: find!(
                value: Id,
                pattern!(space, [{ id @ event::organizer: ?value }])
            )
            .next(),
            sequence: find!(
                value: SequenceValue,
                pattern!(space, [{ id @ event::sequence: ?value }])
            )
            .next(),
            tzid: find!(
                value: TextHandle,
                pattern!(space, [{ id @ event::tzid: ?value }])
            )
            .next(),
            recurrence_id: find!(
                value: IntervalValue,
                pattern!(space, [{ id @ event::recurrence_id: ?value }])
            )
            .next(),
            last_modified: find!(
                value: IntervalValue,
                pattern!(space, [{ id @ event::last_modified: ?value }])
            )
            .next(),
        };
        validate_event_values(&row).is_ok().then_some(row)
    })
}

/// Every readable event carrying this exact UID attachment handle.
pub fn events_with_uid<P>(space: &P, uid: TextHandle) -> Vec<EventRow>
where
    P: TriblePattern + ?Sized,
{
    find!(
        id: Id,
        pattern!(space, [{ ?id @
            metadata::tag: &KIND_EVENT_ID,
            event::ical_uid: uid,
            event::summary: _?summary,
            event::time: _?time,
            event::status: _?status,
            event::transp: _?transp,
        }])
    )
    .filter_map(|id| event(space, id))
    .collect()
}

/// The lineage a version belongs to: its UID and RECURRENCE-ID.
pub type Lineage = (TextHandle, Option<[u8; 32]>);

pub fn lineage(row: &EventRow) -> Lineage {
    (row.uid, row.recurrence_id.map(|value| value.raw))
}

/// Order of versions within one lineage: SEQUENCE, then LAST-MODIFIED (each
/// absent value below any present one), then a version no rereading
/// replaces above one that is replaced, then id. Rereadings are only written
/// between versions that tie on SEQUENCE and LAST-MODIFIED; the id only
/// breaks what remains, so the choice is the same on every replica.
fn version_key(row: &EventRow, reread: &BTreeSet<Id>) -> ([u8; 32], Option<[u8; 32]>, bool, Id) {
    (
        row.sequence.map(|value| value.raw).unwrap_or([0; 32]),
        row.last_modified.map(|value| value.raw),
        !reread.contains(&row.id),
        row.id,
    )
}

/// The current version of every readable lineage. Older versions stay in the
/// collection as what an earlier export said; readers that list or schedule
/// events consult only these.
pub fn current_event_ids<P>(space: &P) -> BTreeSet<Id>
where
    P: TriblePattern + ?Sized,
{
    let reread = reread_event_ids(space);
    let mut current = BTreeMap::<Lineage, EventRow>::new();
    for row in event_ids(space)
        .into_iter()
        .filter_map(|id| event(space, id))
    {
        match current.get(&lineage(&row)) {
            Some(best) if version_key(best, &reread) >= version_key(&row, &reread) => {}
            _ => {
                current.insert(lineage(&row), row);
            }
        }
    }
    current.into_values().map(|row| row.id).collect()
}

/// Every readable version in the lineage of `row`, including `row`.
pub fn lineage_versions<P>(space: &P, row: &EventRow) -> Vec<EventRow>
where
    P: TriblePattern + ?Sized,
{
    events_with_uid(space, row.uid)
        .into_iter()
        .filter(|other| other.recurrence_id == row.recurrence_id)
        .collect()
}

/// Every readable note attached to one event.
pub fn notes_for_event<P>(space: &P, event: Id) -> Vec<NoteRow>
where
    P: TriblePattern + ?Sized,
{
    find!(
        (
            id: Id,
            text: TextHandle,
            created_at: IntervalValue,
        ),
        pattern!(space, [{ ?id @
            metadata::tag: &KIND_NOTE_ID,
            metadata::created_at: ?created_at,
            note::note_about: event,
            note::note_text: ?text,
        }])
    )
    .filter_map(|(id, text, created_at)| {
        validate_point("note creation time", created_at)
            .is_ok()
            .then_some(NoteRow {
                id,
                event,
                text,
                created_at,
            })
    })
    .collect()
}

/// Whether the event has either an RFC cancellation status or a monotone
/// local cancellation assertion. The status is this version's; a local
/// cancellation of any version in its lineage cancels it, so a later export
/// of a cancelled event does not bring it back.
pub fn event_is_cancelled<P>(space: &P, event_id: Id) -> bool
where
    P: TriblePattern + ?Sized,
{
    if exists!(pattern!(space, [{ event_id @
        event::status: STATUS_CANCELLED,
    }])) {
        return true;
    }
    let locally_cancelled = |id: Id| {
        exists!(pattern!(space, [{ _?assertion @
            metadata::tag: &KIND_CANCELLATION_ID,
            cancellation::event: id,
        }]))
    };
    match event(space, event_id) {
        Some(row) => lineage_versions(space, &row)
            .iter()
            .any(|version| locally_cancelled(version.id)),
        None => locally_cancelled(event_id),
    }
}

/// Decode RFC 5545 TEXT escapes (section 3.3.11) for presentation:
/// `\\` is a backslash, `\;` and `\,` are the punctuation, `\n` or `\N`
/// is a newline. Any other backslash is kept as written. Stored values stay
/// as the source wrote them; readers decode when they show summary,
/// location and description.
pub fn unescape_text(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.clone().next() {
            Some(next @ ('\\' | ';' | ',')) => {
                out.push(next);
                chars.next();
            }
            Some('n' | 'N') => {
                out.push('\n');
                chars.next();
            }
            _ => out.push('\\'),
        }
    }
    out
}

/// Strictly read a UTF8String attachment.
pub fn read_text(reader: &impl BlobStoreGet, handle: TextHandle) -> Result<String> {
    let text: anybytes::View<str> = reader.get(handle).context("read Planner text")?;
    Ok(text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(byte: u8) -> Id {
        Id::new([byte; 16]).unwrap()
    }

    fn interval(start: i128, end: i128) -> IntervalValue {
        (
            hifitime::Epoch::from_unix_seconds(start as f64),
            hifitime::Epoch::from_unix_seconds(end as f64),
        )
            .try_to_inline()
            .unwrap()
    }

    fn draft(uid: &str, summary: &str) -> EventDraft {
        EventDraft {
            uid: uid.to_owned(),
            summary: summary.to_owned(),
            description: Some("description".to_owned()),
            time: interval(10, 20),
            rrule: None,
            rdates: BTreeSet::new(),
            exdates: BTreeSet::new(),
            location: Some("room".to_owned()),
            status: STATUS_CONFIRMED.to_owned(),
            transp: TRANSP_OPAQUE.to_owned(),
            attendees: BTreeSet::new(),
            organizer: None,
            sequence: None,
            tzid: None,
            recurrence_id: None,
            last_modified: None,
        }
    }

    #[test]
    fn unescape_text_decodes_rfc5545_text_escapes() {
        assert_eq!(unescape_text(r"SPAM\, SPAM\; SPIT"), "SPAM, SPAM; SPIT");
        assert_eq!(unescape_text(r"one\ntwo\NThree"), "one\ntwo\nThree");
        assert_eq!(unescape_text(r"C:\\temp"), r"C:\temp");
        assert_eq!(
            unescape_text(r"\\n is not a newline"),
            r"\n is not a newline"
        );
        assert_eq!(
            unescape_text(r"keep \x and trailing \"),
            r"keep \x and trailing \"
        );
        assert_eq!(unescape_text("plain"), "plain");
    }

    #[test]
    fn a_rereading_wins_a_tie_whichever_id_is_larger() {
        let mut escaped = draft("tie@example", r"A\, B");
        escaped.last_modified = Some(interval(5, 5));
        let mut decoded = escaped.clone();
        decoded.summary = "A, B".to_owned();
        let escaped = event_fragment(&escaped).unwrap();
        let decoded = event_fragment(&decoded).unwrap();
        let (old, new) = (escaped.root().unwrap(), decoded.root().unwrap());
        for (replaced, replacement) in [(old, new), (new, old)] {
            let mut facts = TribleSet::new();
            facts += escaped.clone().into_facts();
            facts += decoded.clone().into_facts();
            assert_eq!(current_event_ids(&facts).len(), 1);
            facts += rereading_fragment(replaced, replacement).into_facts();
            assert_eq!(
                current_event_ids(&facts),
                BTreeSet::from([replacement]),
                "the later reading is current, not the larger id"
            );
        }
    }

    #[test]
    fn event_identity_is_the_whole_definition() {
        let first = event_fragment(&draft("stable@example", "first")).unwrap();
        let again = event_fragment(&draft("stable@example", "first")).unwrap();
        let edited = event_fragment(&draft("stable@example", "second")).unwrap();
        let other = event_fragment(&draft("other@example", "first")).unwrap();
        let mut stamped = draft("stable@example", "first");
        stamped.last_modified = Some(interval(5, 5));
        let stamped = event_fragment(&stamped).unwrap();

        assert_eq!(first.root(), again.root());
        assert_eq!(first.facts(), again.facts());
        assert_ne!(first.root(), edited.root());
        assert_ne!(first.root(), other.root());
        assert_ne!(first.root(), stamped.root());
    }

    #[test]
    fn cancellation_is_a_monotone_intrinsic_assertion() {
        let event = event_fragment(&draft("cancel@example", "meeting")).unwrap();
        let event_id = event.root().unwrap();
        let cancellation = cancellation_fragment(event_id);
        let mut facts = event.into_facts();
        facts += cancellation.clone().into_facts();

        assert!(event_is_cancelled(&facts, event_id));
        assert_eq!(cancellation_fragment(event_id), cancellation);
    }

    #[test]
    fn baseline_cancelled_is_cancelled_without_assertion() {
        let mut input = draft("baseline@example", "cancelled invite");
        input.status = STATUS_CANCELLED.to_owned();
        let event = event_fragment(&input).unwrap();
        let event_id = event.root().unwrap();

        assert!(event_is_cancelled(event.facts(), event_id));
        assert!(!exists!(pattern!(event.facts(), [{ _?assertion @
            metadata::tag: &KIND_CANCELLATION_ID,
        }])));
    }

    #[test]
    fn ordinary_projection_survives_extra_values() {
        let fragment = event_fragment(&draft("fork@example", "meeting")).unwrap();
        let event_id = fragment.root().unwrap();
        let mut facts = fragment.into_facts();
        facts += entity! { ExclusiveId::force_ref(&event_id) @
            event::summary: "other",
        };

        assert!(event(&facts, event_id).is_some());
    }

    #[test]
    fn ordinary_projection_skips_an_event_lacking_a_required_field() {
        let fragment = event_fragment(&draft("bare@example", "meeting")).unwrap();
        let event_id = fragment.root().unwrap();
        let uid = find!(
            value: TextHandle,
            pattern!(fragment.facts(), [{ event_id @ event::ical_uid: ?value }])
        )
        .next()
        .unwrap();
        let mut facts = entity! {
            metadata::tag: &KIND_EVENT_ID,
            event::ical_uid: uid,
        }
        .into_facts();
        facts += event_fragment(&draft("kept@example", "meeting"))
            .unwrap()
            .into_facts();

        assert_eq!(event_ids(&facts).len(), 1);
        assert!(event(&facts, event_id).is_none());
    }

    #[test]
    fn repeated_fields_are_sets_and_do_not_create_scalar_ambiguity() {
        let mut input = draft("sets@example", "meeting");
        input.attendees = BTreeSet::from([id(1), id(2)]);
        input.rdates = BTreeSet::from([interval(30, 40), interval(50, 60)]);
        let fragment = event_fragment(&input).unwrap();
        let event_id = fragment.root().unwrap();
        let row = event(fragment.facts(), event_id).unwrap();

        assert_eq!(row.attendees, input.attendees);
        assert_eq!(row.rdates, input.rdates);
    }

    #[test]
    fn legacy_random_id_records_are_inert_beside_intrinsic_records() {
        let event = event_fragment(&draft("native@example", "canonical")).unwrap();
        let event_id = event.root().unwrap();
        let uid = find!(
            value: TextHandle,
            pattern!(event.facts(), [{ event_id @ event::ical_uid: ?value }])
        )
        .next()
        .unwrap();
        let legacy_event = id(9);
        let legacy_note = id(10);
        let mut facts = event.into_facts();
        facts += entity! { ExclusiveId::force_ref(&legacy_event) @
            metadata::tag: &KIND_EVENT_ID,
            event::ical_uid: uid,
            event::summary: "legacy",
        };
        facts += entity! { ExclusiveId::force_ref(&legacy_note) @
            metadata::tag: &KIND_NOTE_ID,
            note::note_about: &legacy_event,
        };
        facts += entity! { ExclusiveId::force_ref(&KIND_EVENT_ID) @ metadata::tag: &id(11) };

        assert_eq!(event_ids(&facts), vec![event_id]);
        assert!(notes_for_event(&facts, legacy_event).is_empty());
        assert!(!exists!(pattern!(&facts, [{ _?assertion @
            metadata::tag: &KIND_CANCELLATION_ID,
        }])));
    }
}
