use super::*;

use std::fs::{self, File};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::schemas::planner::{cancellation, KIND_CANCELLATION_ID, KIND_NOTE_ID};

static NEXT_TEST: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let serial = NEXT_TEST.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "faculties-planner-live-{}-{serial}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn fresh_storage(directory: &TestDirectory) -> (PathBuf, PathBuf) {
    let pile = directory.0.join("planner.pile");
    let key = directory.0.join("planner.key");
    File::create(&pile).unwrap();
    crate::storage::initialize_signer(&pile, Some(&key)).unwrap();
    (pile, key)
}

fn fixture_draft(uid: &str, summary: &str) -> EventDraft {
    empty_event_draft(
        uid.to_owned(),
        summary.to_owned(),
        make_interval(
            Epoch::from_unix_seconds(10.0),
            Epoch::from_unix_seconds(20.0),
        ),
        STATUS_CONFIRMED.to_owned(),
        TRANSP_OPAQUE.to_owned(),
    )
}

fn fixture_id(byte: u8) -> Id {
    Id::new([byte; 16]).unwrap()
}

#[test]
fn event_and_initial_note_publish_as_one_signed_mutation() {
    let directory = TestDirectory::new();
    let (pile, key) = fresh_storage(&directory);
    let storage = PlannerStorage {
        storage: &crate::storage::Storage::new(pile.clone(), Some(key.clone())),
    };
    let mut fragment = event_fragment(&fixture_draft("one@example", "meeting")).unwrap();
    let event = fragment.root().unwrap();
    fragment += note_fragment(
        event,
        "agenda",
        point_interval(Epoch::from_unix_seconds(30.0)),
    )
    .unwrap();
    storage
        .update("add event", |_| Ok((Some(fragment), ())))
        .unwrap();

    assert_eq!(storage.payload_count().unwrap(), 1);
    storage
        .with_view(|loaded| {
            assert!(planner_model::event(&loaded.facts, event).is_some());
            assert_eq!(
                planner_model::notes_for_event(&loaded.facts, event).len(),
                1
            );
            let occurrences = collect_occurrences(
                &loaded.reader,
                &loaded.facts,
                (
                    Epoch::from_unix_seconds(0.0),
                    Epoch::from_unix_seconds(40.0),
                ),
                false,
            );
            assert_eq!(occurrences.len(), 1);
            assert_eq!(occurrences[0].event_id, event);
            Ok(())
        })
        .unwrap();
}

#[test]
fn same_batch_duplicate_collapses_and_changed_definition_is_a_revision() {
    let directory = TestDirectory::new();
    let (pile, key) = fresh_storage(&directory);
    let storage = PlannerStorage {
        storage: &crate::storage::Storage::new(pile.clone(), Some(key.clone())),
    };
    storage
        .with_view(|loaded| {
            let mut staged = BTreeMap::new();
            let first = event_fragment(&fixture_draft("duplicate@example", "same")).unwrap();
            let duplicate = event_fragment(&fixture_draft("duplicate@example", "same")).unwrap();
            let changed = event_fragment(&fixture_draft("duplicate@example", "different")).unwrap();
            assert_ne!(first.root(), changed.root(), "identity covers the content");

            assert!(matches!(
                stage_import_event(loaded, &mut staged, &mut Vec::new(), first).unwrap(),
                Staged::New
            ));
            assert!(matches!(
                stage_import_event(loaded, &mut staged, &mut Vec::new(), duplicate).unwrap(),
                Staged::Duplicate
            ));
            assert!(matches!(
                stage_import_event(loaded, &mut staged, &mut Vec::new(), changed).unwrap(),
                Staged::Revision
            ));
            assert_eq!(staged.len(), 2);
            Ok(())
        })
        .unwrap();
}

#[test]
fn duplicate_scalar_ical_property_is_rejected() {
    let bytes = b"BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:a@example\r\nUID:b@example\r\nDTSTART:20260809T120000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    let calendars: Vec<_> = ical::IcalParser::new(&bytes[..]).collect();
    let calendar: Vec<_> = calendars
        .into_iter()
        .map(|calendar| calendar.unwrap())
        .collect();
    let error = parse_ical_event(
        &calendar[0].events[0],
        &ZoneTable::new(),
        IngestOptions::default(),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("more than one UID"));
}

#[test]
fn exact_reingest_does_not_publish_another_authored_commit() {
    let directory = TestDirectory::new();
    let (pile, key) = fresh_storage(&directory);
    let ics = directory.0.join("event.ics");
    fs::write(
        &ics,
        "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:stable@example\r\nSUMMARY:Stable\r\nDTSTART:20260809T120000Z\r\nDTEND:20260809T130000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
    )
    .unwrap();
    let storage = PlannerStorage {
        storage: &crate::storage::Storage::new(pile.clone(), Some(key.clone())),
    };

    let text = fs::read_to_string(&ics).unwrap();
    let documents = [CalendarInput {
        name: "event.ics",
        text: &text,
    }];
    ingest(storage, &documents, IngestOptions::default()).unwrap();
    ingest(storage, &documents, IngestOptions::default()).unwrap();

    assert_eq!(storage.payload_count().unwrap(), 1);
}

#[test]
fn two_definitions_of_one_uid_in_a_batch_are_both_kept() {
    let directory = TestDirectory::new();
    let (pile, key) = fresh_storage(&directory);
    let planner = Planner::new(pile, Some(key));
    let text = vevent_calendar(&format!(
        "{}{}",
        vevent("fork@example", &["DTSTART:20260809T120000Z", "SEQUENCE:0"]),
        vevent("fork@example", &["DTSTART:20260809T140000Z", "SEQUENCE:1"]),
    ));
    let receipt = planner
        .ingest(
            &[CalendarInput {
                name: "fork.ics",
                text: &text,
            }],
            IngestOptions::default(),
        )
        .unwrap();
    assert_eq!(
        (
            receipt.imported.len(),
            receipt.revisions,
            receipt.duplicates
        ),
        (2, 1, 0)
    );
    let starts = starts_in(&planner, "2026-08-09T00:00:00Z", "2026-08-10T00:00:00Z");
    assert_eq!(
        starts,
        vec![("fork@example".to_owned(), utc("2026-08-09T14:00:00Z"))]
    );
}

#[test]
fn cancel_adds_one_assertion_without_mutating_baseline_status() {
    let directory = TestDirectory::new();
    let (pile, key) = fresh_storage(&directory);
    let storage = PlannerStorage {
        storage: &crate::storage::Storage::new(pile.clone(), Some(key.clone())),
    };
    let event = event_fragment(&fixture_draft("cancel@example", "meeting")).unwrap();
    let event_id = event.root().unwrap();
    storage
        .update("add event", |_| Ok((Some(event), ())))
        .unwrap();

    cancel(storage, &fmt_id(event_id)).unwrap();
    cancel(storage, &fmt_id(event_id)).unwrap();

    assert_eq!(storage.payload_count().unwrap(), 2);
    storage
        .with_view(|loaded| {
            assert_eq!(
                planner_model::event(&loaded.facts, event_id)
                    .expect("published event remains readable")
                    .status,
                planner_model::text_handle(STATUS_CONFIRMED)
            );
            assert!(planner_model::event_is_cancelled(&loaded.facts, event_id));
            Ok(())
        })
        .unwrap();
}

#[test]
fn opaque_repeated_cancellation_edges_cancel_every_occurrence_target() {
    let first = event_fragment(&fixture_draft("first@example", "first")).unwrap();
    let first_id = first.root().unwrap();
    let second = event_fragment(&fixture_draft("second@example", "second")).unwrap();
    let second_id = second.root().unwrap();
    let assertion = fixture_id(7);
    let mut payloads = first + second;
    let mut facts = payloads.facts().clone();
    let reader = payloads.blobs_mut().snapshot().unwrap();
    facts += entity! { ExclusiveId::force_ref(&assertion) @
        metadata::tag: &KIND_CANCELLATION_ID,
        cancellation::event: &first_id,
    };
    facts += entity! { ExclusiveId::force_ref(&assertion) @
        cancellation::event: &second_id,
    };

    let window = (
        Epoch::from_unix_seconds(0.0),
        Epoch::from_unix_seconds(30.0),
    );
    assert!(collect_occurrences(&reader, &facts, window, false).is_empty());

    let all = collect_occurrences(&reader, &facts, window, true);
    assert_eq!(all.len(), 2);
    assert!(all
        .iter()
        .all(|occurrence| occurrence.status == STATUS_CANCELLED));
    assert_eq!(
        all.iter()
            .map(|occurrence| occurrence.event_id)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([first_id, second_id])
    );
}

#[test]
fn occurrence_reads_ignore_notes_and_skip_unsupported_event_rows() {
    let event = event_fragment(&fixture_draft("readable@example", "readable")).unwrap();
    let event_id = event.root().unwrap();
    let mut unsupported = fixture_draft("unsupported@example", "unsupported");
    unsupported.rrule = Some("FREQ=NOTREAL".to_owned());
    let unsupported = event_fragment(&unsupported).unwrap();
    let note = note_fragment(
        event_id,
        "this belongs to show, not occurrence reads",
        point_interval(Epoch::from_unix_seconds(30.0)),
    )
    .unwrap();
    let mut payloads = event + unsupported + note;
    let mut facts = payloads.facts().clone();
    let reader = payloads.blobs_mut().snapshot().unwrap();
    let incomplete_note = fixture_id(8);
    facts += entity! { ExclusiveId::force_ref(&incomplete_note) @
        metadata::tag: &KIND_NOTE_ID,
    };

    let window = (
        Epoch::from_unix_seconds(0.0),
        Epoch::from_unix_seconds(30.0),
    );
    let occurrences = collect_occurrences(&reader, &facts, window, false);
    assert_eq!(occurrences.len(), 1);
    assert_eq!(occurrences[0].event_id, event_id);
    assert_eq!(
        next_occurrence(&reader, &facts, window)
            .expect("readable event remains the next occurrence")
            .event_id,
        event_id
    );
}

#[test]
fn next_compares_rrule_and_every_rdate_after_applying_exdates() {
    let mut recurring = fixture_draft("recurring@example", "recurring");
    recurring.time = make_interval(
        Epoch::from_unix_seconds(0.0),
        Epoch::from_unix_seconds(10.0),
    );
    recurring.rrule = Some("FREQ=DAILY;COUNT=3".to_owned());
    recurring.rdates = BTreeSet::from([
        make_interval(
            Epoch::from_unix_seconds(50.0),
            Epoch::from_unix_seconds(60.0),
        ),
        make_interval(
            Epoch::from_unix_seconds(90_000.0),
            Epoch::from_unix_seconds(90_010.0),
        ),
    ]);
    recurring.exdates = BTreeSet::from([
        recurring.time,
        make_interval(
            Epoch::from_unix_seconds(50.0),
            Epoch::from_unix_seconds(60.0),
        ),
    ]);
    let recurring = event_fragment(&recurring).unwrap();
    let recurring_id = recurring.root().unwrap();

    let mut later = fixture_draft("later@example", "later");
    later.time = make_interval(
        Epoch::from_unix_seconds(100_000.0),
        Epoch::from_unix_seconds(100_010.0),
    );
    let later = event_fragment(&later).unwrap();
    let mut payloads = recurring + later;
    let facts = payloads.facts().clone();
    let reader = payloads.blobs_mut().snapshot().unwrap();

    // EXDATE must be inside the recurrence set before `all(1)`: otherwise
    // it yields the excluded DTSTART and the later RDATE wins after the
    // exact filter. The next unexcluded RRULE occurrence is one day later.
    let next = next_occurrence(
        &reader,
        &facts,
        (
            Epoch::from_unix_seconds(-1.0),
            Epoch::from_unix_seconds(200_000.0),
        ),
    )
    .expect("an unexcluded occurrence exists");
    assert_eq!(next.event_id, recurring_id);
    assert_eq!(next.start, Epoch::from_unix_seconds(86_400.0));
    assert_eq!(next.end, Epoch::from_unix_seconds(86_410.0));

    // The RDATE fold must inspect every value rather than assume query
    // order. Put the winning value after a later one deliberately.
    let mut row = occurrence_event(&reader, &facts, recurring_id).unwrap();
    row.rdates = vec![
        (
            Epoch::from_unix_seconds(90_000.0),
            Epoch::from_unix_seconds(90_010.0),
        ),
        (
            Epoch::from_unix_seconds(100.0),
            Epoch::from_unix_seconds(110.0),
        ),
    ];
    assert_eq!(
        next_event_occurrence(
            &row,
            (
                Epoch::from_unix_seconds(1.0),
                Epoch::from_unix_seconds(200_000.0),
            ),
        ),
        Some((
            Epoch::from_unix_seconds(100.0),
            Epoch::from_unix_seconds(110.0),
        ))
    );
}

// --- iCalendar time zones (TZID, floating, DST, overrides) ---

fn vevent_calendar(body: &str) -> String {
    format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\n{body}END:VCALENDAR\r\n")
}

fn vevent(uid: &str, lines: &[&str]) -> String {
    let mut text = format!("BEGIN:VEVENT\r\nUID:{uid}\r\nSUMMARY:{uid}\r\n");
    for line in lines {
        text.push_str(line);
        text.push_str("\r\n");
    }
    text.push_str("END:VEVENT\r\n");
    text
}

/// Parse every VEVENT of one document the way `ingest` does.
fn parse_document(text: &str, options: IngestOptions) -> Result<Vec<IcalEvent>> {
    let mut events = Vec::new();
    for calendar in ical::IcalParser::new(text.as_bytes()) {
        let calendar = calendar?;
        let zones = calendar_zones(&calendar);
        for source in &calendar.events {
            events.push(parse_ical_event(source, &zones, options)?);
        }
    }
    Ok(events)
}

fn utc(text: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(text)
        .unwrap()
        .with_timezone(&Utc)
}

fn berlin() -> IngestOptions {
    IngestOptions {
        floating_zone: Some(chrono_tz::Europe::Berlin),
    }
}

#[test]
fn tzid_berlin_resolves_winter_and_summer_offsets() {
    let text = vevent_calendar(&format!(
        "{}{}",
        vevent(
            "winter@example",
            &[
                "DTSTART;TZID=Europe/Berlin:20260115T100000",
                "DTEND;TZID=Europe/Berlin:20260115T110000"
            ]
        ),
        vevent(
            "summer@example",
            &[
                "DTSTART;TZID=Europe/Berlin:20260715T100000",
                "DTEND;TZID=Europe/Berlin:20260715T110000"
            ]
        ),
    ));
    let events = parse_document(&text, IngestOptions::default()).unwrap();
    assert_eq!(events[0].dtstart, utc("2026-01-15T09:00:00Z"));
    assert_eq!(events[0].dtend, utc("2026-01-15T10:00:00Z"));
    assert_eq!(events[0].zone, Some(chrono_tz::Europe::Berlin));
    assert_eq!(events[1].dtstart, utc("2026-07-15T08:00:00Z"));
    assert_eq!(events[1].dtend, utc("2026-07-15T09:00:00Z"));
}

#[test]
fn utc_and_date_values_are_unchanged_and_carry_no_zone() {
    let text = vevent_calendar(&format!(
        "{}{}",
        vevent("z@example", &["DTSTART:20260809T120000Z"]),
        vevent("day@example", &["DTSTART;VALUE=DATE:20260809"]),
    ));
    let events = parse_document(&text, IngestOptions::default()).unwrap();
    assert_eq!(events[0].dtstart, utc("2026-08-09T12:00:00Z"));
    assert_eq!(events[0].zone, None);
    assert_eq!(events[1].dtstart, utc("2026-08-09T00:00:00Z"));
    assert_eq!(events[1].dtend, utc("2026-08-10T00:00:00Z"));
    assert_eq!(events[1].zone, None);
}

#[test]
fn dst_gap_moves_forward_and_overlap_takes_the_first_occurrence() {
    // 02:30 does not exist in Berlin on 2026-03-29: read with the offset
    // before the gap (+01:00), so it is 01:30Z, which is 03:30 CEST.
    let gap = NaiveDate::from_ymd_opt(2026, 3, 29)
        .unwrap()
        .and_hms_opt(2, 30, 0)
        .unwrap();
    assert_eq!(
        resolve_local(chrono_tz::Europe::Berlin, gap),
        utc("2026-03-29T01:30:00Z")
    );
    // 02:30 happens twice on 2026-10-25: the first is still CEST (+02:00).
    let overlap = NaiveDate::from_ymd_opt(2026, 10, 25)
        .unwrap()
        .and_hms_opt(2, 30, 0)
        .unwrap();
    assert_eq!(
        resolve_local(chrono_tz::Europe::Berlin, overlap),
        utc("2026-10-25T00:30:00Z")
    );
}

#[test]
fn unknown_tzid_is_an_error_naming_the_event() {
    let text = vevent_calendar(&vevent(
        "windows@example",
        &["DTSTART;TZID=W. Europe Standard Time:20260115T100000"],
    ));
    let error = format!(
        "{:#}",
        parse_document(&text, IngestOptions::default()).unwrap_err()
    );
    assert!(error.contains("windows@example"), "{error}");
    assert!(
        error.contains("unknown TZID 'W. Europe Standard Time'"),
        "{error}"
    );
}

#[test]
fn fixed_offset_vtimezone_resolves_and_varying_one_does_not() {
    let fixed = "BEGIN:VTIMEZONE\r\nTZID:GMT-0700\r\nBEGIN:STANDARD\r\nDTSTART:20010101T000000\r\nTZOFFSETFROM:-0700\r\nTZOFFSETTO:-0700\r\nEND:STANDARD\r\nEND:VTIMEZONE\r\n";
    let text = vevent_calendar(&format!(
        "{fixed}{}",
        vevent(
            "fixed@example",
            &[
                "DTSTART:20160701T170000Z",
                "DTEND;TZID=GMT-0700:20160701T114500"
            ]
        )
    ));
    let events = parse_document(&text, IngestOptions::default()).unwrap();
    assert_eq!(events[0].dtend, utc("2016-07-01T18:45:00Z"));

    let varying = "BEGIN:VTIMEZONE\r\nTZID:Custom\r\nBEGIN:STANDARD\r\nDTSTART:20010101T000000\r\nTZOFFSETFROM:+0200\r\nTZOFFSETTO:+0100\r\nEND:STANDARD\r\nEND:VTIMEZONE\r\n";
    let text = vevent_calendar(&format!(
        "{varying}{}",
        vevent("custom@example", &["DTSTART;TZID=Custom:20160701T114500"])
    ));
    let error = format!(
        "{:#}",
        parse_document(&text, IngestOptions::default()).unwrap_err()
    );
    assert!(
        error.contains("custom@example") && error.contains("unknown TZID 'Custom'"),
        "{error}"
    );
}

#[test]
fn floating_time_needs_a_zone_and_is_never_utc() {
    let text = vevent_calendar(&vevent("floating@example", &["DTSTART:20260115T100000"]));
    let error = format!(
        "{:#}",
        parse_document(&text, IngestOptions::default()).unwrap_err()
    );
    assert!(
        error.contains("floating@example") && error.contains("floating time"),
        "{error}"
    );

    let events = parse_document(&text, berlin()).unwrap();
    assert_eq!(events[0].dtstart, utc("2026-01-15T09:00:00Z"));
    assert_eq!(events[0].zone, Some(chrono_tz::Europe::Berlin));
}

#[test]
fn local_until_is_rewritten_to_utc_in_the_event_zone() {
    assert_eq!(
        utc_until("FREQ=WEEKLY;UNTIL=20171005", chrono_tz::Europe::Berlin),
        "FREQ=WEEKLY;UNTIL=20171005T215959Z"
    );
    assert_eq!(
        utc_until(
            "FREQ=WEEKLY;UNTIL=20251120T123000Z;BYDAY=TH",
            chrono_tz::Europe::Berlin
        ),
        "FREQ=WEEKLY;UNTIL=20251120T123000Z;BYDAY=TH"
    );
}

fn starts_in(planner: &Planner, from: &str, to: &str) -> Vec<(String, DateTime<Utc>)> {
    planner
        .list(
            (chrono_to_epoch(utc(from)), chrono_to_epoch(utc(to))),
            false,
        )
        .unwrap()
        .into_iter()
        .map(|occurrence| {
            (
                occurrence.summary,
                epoch_to_chrono_utc(occurrence.start).unwrap(),
            )
        })
        .collect()
}

#[test]
fn rrule_keeps_local_wall_time_across_the_october_dst_change() {
    let directory = TestDirectory::new();
    let (pile, key) = fresh_storage(&directory);
    let planner = Planner::new(pile, Some(key));
    let text = vevent_calendar(&vevent(
        "weekly@example",
        &[
            "DTSTART;TZID=Europe/Berlin:20261015T100000",
            "DTEND;TZID=Europe/Berlin:20261015T110000",
            "RRULE:FREQ=WEEKLY;COUNT=4",
        ],
    ));
    planner
        .ingest(
            &[CalendarInput {
                name: "weekly.ics",
                text: &text,
            }],
            IngestOptions::default(),
        )
        .unwrap();
    let starts = starts_in(&planner, "2026-10-01T00:00:00Z", "2026-12-01T00:00:00Z")
        .into_iter()
        .map(|(_, start)| start)
        .collect::<Vec<_>>();
    assert_eq!(
        starts,
        vec![
            utc("2026-10-15T08:00:00Z"),
            utc("2026-10-22T08:00:00Z"),
            utc("2026-10-29T09:00:00Z"),
            utc("2026-11-05T09:00:00Z"),
        ]
    );
}

#[test]
fn exdate_and_recurrence_id_override_replace_master_instances() {
    let directory = TestDirectory::new();
    let (pile, key) = fresh_storage(&directory);
    let planner = Planner::new(pile, Some(key));
    let master = vevent(
        "series@example",
        &[
            "DTSTART;TZID=Europe/Berlin:20261015T100000",
            "DTEND;TZID=Europe/Berlin:20261015T110000",
            "RRULE:FREQ=WEEKLY;COUNT=4",
            "EXDATE;TZID=Europe/Berlin:20261022T100000",
        ],
    );
    let moved = "BEGIN:VEVENT\r\nUID:series@example\r\nSUMMARY:moved\r\nRECURRENCE-ID;TZID=Europe/Berlin:20261029T100000\r\nDTSTART;TZID=Europe/Berlin:20261029T140000\r\nDTEND;TZID=Europe/Berlin:20261029T150000\r\nEND:VEVENT\r\n";
    let text = vevent_calendar(&format!("{master}{moved}"));
    let documents = [CalendarInput {
        name: "series.ics",
        text: &text,
    }];
    let receipt = planner
        .ingest(&documents, IngestOptions::default())
        .unwrap();
    assert_eq!((receipt.imported.len(), receipt.total), (2, 2));
    assert_eq!(
        starts_in(&planner, "2026-10-01T00:00:00Z", "2026-12-01T00:00:00Z"),
        vec![
            ("series@example".to_owned(), utc("2026-10-15T08:00:00Z")),
            ("moved".to_owned(), utc("2026-10-29T13:00:00Z")),
            ("series@example".to_owned(), utc("2026-11-05T09:00:00Z")),
        ]
    );
    // Re-ingesting the same document is a pair of exact duplicates.
    let again = planner
        .ingest(&documents, IngestOptions::default())
        .unwrap();
    assert_eq!((again.imported.len(), again.duplicates), (0, 2));
    // The next occurrence after the first skips both the EXDATE and the
    // replaced instance.
    let next = planner
        .next(Some(chrono_to_epoch(utc("2026-10-16T00:00:00Z"))))
        .unwrap()
        .unwrap();
    assert_eq!(next.summary, "moved");
}

// --- Re-export: changed, moved and deleted events (open world) ---

fn ingest_text(planner: &Planner, text: &str) -> IngestReceipt {
    planner
        .ingest(
            &[CalendarInput {
                name: "export.ics",
                text,
            }],
            IngestOptions::default(),
        )
        .unwrap()
}

#[test]
fn reexport_with_a_moved_event_records_a_new_version_and_lists_only_it() {
    let directory = TestDirectory::new();
    let (pile, key) = fresh_storage(&directory);
    let planner = Planner::new(pile, Some(key));
    let first = vevent_calendar(&format!(
        "{}{}",
        vevent(
            "dentist@example",
            &[
                "DTSTART:20261120T090000Z",
                "DTEND:20261120T100000Z",
                "SEQUENCE:0",
                "LAST-MODIFIED:20261101T120000Z",
                "DTSTAMP:20261119T082419Z",
            ],
        ),
        vevent(
            "lunch@example",
            &[
                "DTSTART:20261121T110000Z",
                "SEQUENCE:0",
                "DTSTAMP:20261119T082419Z",
            ],
        ),
    ));
    let receipt = ingest_text(&planner, &first);
    assert_eq!((receipt.imported.len(), receipt.revisions), (2, 0));

    // The second export moved the dentist, left lunch alone, and has a new
    // DTSTAMP everywhere, which must not make lunch a new version.
    let second = vevent_calendar(&format!(
        "{}{}",
        vevent(
            "dentist@example",
            &[
                "DTSTART:20261120T140000Z",
                "DTEND:20261120T150000Z",
                "SEQUENCE:1",
                "LAST-MODIFIED:20261119T100000Z",
                "DTSTAMP:20261126T082419Z",
            ],
        ),
        vevent(
            "lunch@example",
            &[
                "DTSTART:20261121T110000Z",
                "SEQUENCE:0",
                "DTSTAMP:20261126T082419Z",
            ],
        ),
    ));
    let receipt = ingest_text(&planner, &second);
    assert_eq!(
        (
            receipt.imported.len(),
            receipt.revisions,
            receipt.duplicates
        ),
        (1, 1, 1)
    );
    assert_eq!(
        starts_in(&planner, "2026-11-20T00:00:00Z", "2026-11-22T00:00:00Z"),
        vec![
            ("dentist@example".to_owned(), utc("2026-11-20T14:00:00Z")),
            ("lunch@example".to_owned(), utc("2026-11-21T11:00:00Z")),
        ]
    );

    // Re-ingesting the first export afterwards adds nothing and does not
    // move the dentist back: the newer SEQUENCE stays current.
    let receipt = ingest_text(&planner, &first);
    assert_eq!((receipt.imported.len(), receipt.duplicates), (0, 2));
    assert_eq!(
        starts_in(&planner, "2026-11-20T00:00:00Z", "2026-11-21T00:00:00Z"),
        vec![("dentist@example".to_owned(), utc("2026-11-20T14:00:00Z"))]
    );
}

#[test]
fn version_order_is_sequence_then_last_modified_not_file_order() {
    let directory = TestDirectory::new();
    let (pile, key) = fresh_storage(&directory);
    let planner = Planner::new(pile, Some(key));
    let newer = vevent(
        "edited@example",
        &[
            "DTSTART:20261120T150000Z",
            "SEQUENCE:0",
            "LAST-MODIFIED:20261110T000000Z",
        ],
    );
    let older = vevent(
        "edited@example",
        &[
            "DTSTART:20261120T090000Z",
            "SEQUENCE:0",
            "LAST-MODIFIED:20261101T000000Z",
        ],
    );
    // The newer version arrives first; LAST-MODIFIED decides at equal SEQUENCE.
    ingest_text(&planner, &vevent_calendar(&newer));
    ingest_text(&planner, &vevent_calendar(&older));
    assert_eq!(
        starts_in(&planner, "2026-11-20T00:00:00Z", "2026-11-21T00:00:00Z"),
        vec![("edited@example".to_owned(), utc("2026-11-20T15:00:00Z"))]
    );
    // A higher SEQUENCE wins over a later LAST-MODIFIED.
    ingest_text(
        &planner,
        &vevent_calendar(&vevent(
            "edited@example",
            &[
                "DTSTART:20261120T070000Z",
                "SEQUENCE:2",
                "LAST-MODIFIED:20261105T000000Z",
            ],
        )),
    );
    assert_eq!(
        starts_in(&planner, "2026-11-20T00:00:00Z", "2026-11-21T00:00:00Z"),
        vec![("edited@example".to_owned(), utc("2026-11-20T07:00:00Z"))]
    );
}

#[test]
fn a_new_exdate_and_a_new_override_on_reexport_are_revisions() {
    let directory = TestDirectory::new();
    let (pile, key) = fresh_storage(&directory);
    let planner = Planner::new(pile, Some(key));
    let master = |extra: &[&str]| {
        let mut lines = vec![
            "DTSTART;TZID=Europe/Berlin:20261015T100000",
            "DTEND;TZID=Europe/Berlin:20261015T110000",
            "RRULE:FREQ=WEEKLY;COUNT=3",
        ];
        lines.extend_from_slice(extra);
        vevent("series@example", &lines)
    };
    ingest_text(&planner, &vevent_calendar(&master(&["SEQUENCE:0"])));
    let override_ = "BEGIN:VEVENT\r\nUID:series@example\r\nSUMMARY:moved\r\nRECURRENCE-ID;TZID=Europe/Berlin:20261029T100000\r\nDTSTART;TZID=Europe/Berlin:20261029T140000\r\nDTEND;TZID=Europe/Berlin:20261029T150000\r\nEND:VEVENT\r\n";
    let receipt = ingest_text(
        &planner,
        &vevent_calendar(&format!(
            "{}{override_}",
            master(&["SEQUENCE:1", "EXDATE;TZID=Europe/Berlin:20261022T100000"])
        )),
    );
    assert_eq!(
        (receipt.imported.len(), receipt.revisions),
        (2, 1),
        "the master is a revision, the override a new lineage"
    );
    assert_eq!(
        starts_in(&planner, "2026-10-01T00:00:00Z", "2026-12-01T00:00:00Z"),
        vec![
            ("series@example".to_owned(), utc("2026-10-15T08:00:00Z")),
            ("moved".to_owned(), utc("2026-10-29T13:00:00Z")),
        ]
    );
}

#[test]
fn a_local_cancellation_holds_for_later_versions_and_old_versions_stay_readable() {
    let directory = TestDirectory::new();
    let (pile, key) = fresh_storage(&directory);
    let planner = Planner::new(pile, Some(key));
    let first = ingest_text(
        &planner,
        &vevent_calendar(&vevent(
            "party@example",
            &["DTSTART:20261120T180000Z", "SEQUENCE:0"],
        )),
    );
    let old = first.imported[0];
    planner.note(&format!("{old:x}"), "bring cake").unwrap();
    planner.cancel(&format!("{old:x}")).unwrap();
    let second = ingest_text(
        &planner,
        &vevent_calendar(&vevent(
            "party@example",
            &["DTSTART:20261120T190000Z", "SEQUENCE:1"],
        )),
    );
    let new = second.imported[0];
    assert!(starts_in(&planner, "2026-11-20T00:00:00Z", "2026-11-21T00:00:00Z").is_empty());
    let current = planner.show(&format!("{new:x}")).unwrap();
    assert!(current.cancelled);
    assert_eq!(current.notes.len(), 1, "notes belong to the lineage");
    // The superseded version is still what the first export said.
    let shown = planner.show(&format!("{old:x}")).unwrap();
    assert_eq!(
        epoch_to_chrono_utc(
            shown
                .event
                .time
                .try_from_inline::<(Epoch, Epoch)>()
                .unwrap()
                .0
        )
        .unwrap(),
        utc("2026-11-20T18:00:00Z")
    );
}

#[test]
fn an_event_missing_from_a_later_export_is_not_removed() {
    let directory = TestDirectory::new();
    let (pile, key) = fresh_storage(&directory);
    let planner = Planner::new(pile, Some(key));
    ingest_text(
        &planner,
        &vevent_calendar(&format!(
            "{}{}",
            vevent("kept@example", &["DTSTART:20261120T090000Z"]),
            vevent("deleted@example", &["DTSTART:20261120T100000Z"]),
        )),
    );
    ingest_text(
        &planner,
        &vevent_calendar(&vevent("kept@example", &["DTSTART:20261120T090000Z"])),
    );
    // Deletion needs its own observation (which versions an export held);
    // until then an absent event stays, which is the honest default.
    assert_eq!(
        starts_in(&planner, "2026-11-20T00:00:00Z", "2026-11-21T00:00:00Z").len(),
        2
    );
}

#[test]
fn ingest_unescapes_text_and_a_manual_backslash_is_kept() {
    let directory = TestDirectory::new();
    let (pile, key) = fresh_storage(&directory);
    let planner = Planner::new(pile, Some(key));
    let text = vevent_calendar(
        "BEGIN:VEVENT\r\nUID:esc@example\r\nSUMMARY:SPAM\\, SPAM\\; SPIT\r\nLOCATION:Saal\\, MP7\\nOG\r\nDESCRIPTION:line\\nnext\\\\done\r\nDTSTART:20260809T120000Z\r\nEND:VEVENT\r\n",
    );
    planner
        .ingest(
            &[CalendarInput {
                name: "esc.ics",
                text: &text,
            }],
            IngestOptions::default(),
        )
        .unwrap();
    let mut manual = AddOptions::new(
        r"C:\new\temp, keep\,this",
        (
            chrono_to_epoch(utc("2026-08-10T12:00:00Z")),
            chrono_to_epoch(utc("2026-08-10T13:00:00Z")),
        ),
    );
    manual.location = Some(r"room\n2".to_owned());
    let added = planner.add(&manual).unwrap();

    let listed = planner
        .list(
            (
                chrono_to_epoch(utc("2026-08-01T00:00:00Z")),
                chrono_to_epoch(utc("2026-08-31T00:00:00Z")),
            ),
            false,
        )
        .unwrap();
    let imported = listed
        .iter()
        .find(|occurrence| occurrence.event_id != added.event)
        .unwrap();
    assert_eq!(imported.summary, "SPAM, SPAM; SPIT");
    assert_eq!(imported.location.as_deref(), Some("Saal, MP7 OG"));
    let detail = planner.show(&format!("{:x}", imported.event_id)).unwrap();
    assert_eq!(detail.description.as_deref(), Some("line\nnext\\done"));

    let manual = planner.show(&format!("{:x}", added.event)).unwrap();
    assert_eq!(manual.summary, r"C:\new\temp, keep\,this");
    assert_eq!(manual.location.as_deref(), Some(r"room\n2"));
}

#[test]
fn reingest_over_an_escaped_reading_makes_the_unescaped_version_current() {
    let directory = TestDirectory::new();
    let (pile, key) = fresh_storage(&directory);
    let planner = Planner::new(pile, Some(key));
    // What the reader before this change stored for the same VEVENT: the
    // escapes kept, with the same SEQUENCE and LAST-MODIFIED.
    let mut escaped = empty_event_draft(
        "tie@example".to_owned(),
        r"A\, B".to_owned(),
        make_interval(
            chrono_to_epoch(utc("2026-08-09T12:00:00Z")),
            chrono_to_epoch(utc("2026-08-09T13:00:00Z")),
        ),
        STATUS_CONFIRMED.to_owned(),
        TRANSP_OPAQUE.to_owned(),
    );
    escaped.location = Some(r"Saal\, OG".to_owned());
    escaped.sequence = Some(3u64.to_inline());
    let modified = chrono_to_epoch(utc("2026-08-01T00:00:00Z"));
    escaped.last_modified = Some(make_interval(modified, modified));
    let escaped = event_fragment(&escaped).unwrap();
    let escaped_id = escaped.root().unwrap();
    planner
        .storage()
        .update("seed escaped reading", |_| Ok((Some(escaped), ())))
        .unwrap();

    let text = vevent_calendar(
        "BEGIN:VEVENT\r\nUID:tie@example\r\nSUMMARY:A\\, B\r\nLOCATION:Saal\\, OG\r\nSEQUENCE:3\r\nLAST-MODIFIED:20260801T000000Z\r\nDTSTART:20260809T120000Z\r\nDTEND:20260809T130000Z\r\nEND:VEVENT\r\n",
    );
    let documents = [CalendarInput {
        name: "tie.ics",
        text: &text,
    }];
    let receipt = planner
        .ingest(&documents, IngestOptions::default())
        .unwrap();
    assert_eq!(receipt.revisions, 1);
    let decoded_id = receipt.imported[0];
    assert_ne!(decoded_id, escaped_id);

    let listed = starts_in(&planner, "2026-08-01T00:00:00Z", "2026-08-31T00:00:00Z");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].0, "A, B");
    let next = planner
        .list(
            (
                chrono_to_epoch(utc("2026-08-01T00:00:00Z")),
                chrono_to_epoch(utc("2026-08-31T00:00:00Z")),
            ),
            false,
        )
        .unwrap();
    assert_eq!(next[0].event_id, decoded_id);
    assert_eq!(next[0].location.as_deref(), Some("Saal, OG"));

    // The same export again is a duplicate and writes nothing more.
    let again = planner
        .ingest(&documents, IngestOptions::default())
        .unwrap();
    assert_eq!(again.duplicates, 1);
    assert!(again.imported.is_empty());
}
