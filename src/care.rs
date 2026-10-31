//! Care: health observations, one immutable record per reading.
//!
//! Records and displays only. Nothing here flags a range, interprets a value
//! or advises.
//!
//! An observation is a private relation built with `entity!` at the point of
//! writing. Its archive (the observation plus the kind, unit and method
//! entities it names) and its note and photo are sealed as ONE Secrets record
//! (`secrets::record::seal`). Each record is delivered under a Secrets group
//! resource per (subject, kind), so one Secrets grant shares a subject's
//! readings of that kind, including later ones. Care adds no crypto and no
//! grant format: envelopes, grants and key delivery are the Secrets ones.
//!
//! Readers open every record their key can open, union the archives into one
//! ordinary `TribleSet`, and query it at the point of use.

pub mod cli;
pub mod mcp;
pub mod operations;
pub mod render;

pub use operations::{AddInput, Added, Care, Row, Table};

use anybytes::View;
use anyhow::{anyhow, bail, Result};
use hifitime::Epoch;
use triblespace::core::metadata;
use triblespace::prelude::*;

use crate::schemas::care::*;

pub type IntervalValue = Inline<inlineencodings::NsTAIInterval>;
pub type TextHandle = Inline<inlineencodings::Handle<blobencodings::UTF8String>>;
pub type NoteHandle = Inline<
    inlineencodings::Handle<crate::secrets::encrypted::Encrypted<blobencodings::UTF8String>>,
>;
pub type PhotoHandle =
    Inline<inlineencodings::Handle<crate::secrets::encrypted::Encrypted<blobencodings::RawBytes>>>;

/// Public label of a sealed observation version (the label is not encrypted).
pub const RECORD_LABEL: &str = "care observation";
/// Public label of the Secrets group a subject's readings of one kind share.
pub const GROUP_LABEL: &str = "care group";

/// One seed vocabulary term: id, display name, short alias, code annotation.
pub struct Term {
    pub id: Id,
    pub name: &'static str,
    pub alias: &'static str,
    pub code: Option<&'static str>,
}

pub const KINDS: &[Term] = &[
    Term {
        id: KIND_SYSTOLIC_BP,
        name: "systolic blood pressure",
        alias: "systolic",
        code: Some("8480-6"),
    },
    Term {
        id: KIND_DIASTOLIC_BP,
        name: "diastolic blood pressure",
        alias: "diastolic",
        code: Some("8462-4"),
    },
    Term {
        id: KIND_HEART_RATE,
        name: "heart rate",
        alias: "heart-rate",
        code: Some("8867-4"),
    },
    Term {
        id: KIND_BODY_TEMPERATURE,
        name: "body temperature",
        alias: "temperature",
        code: Some("8310-5"),
    },
    Term {
        id: KIND_URINE_OUTPUT,
        name: "urine output",
        alias: "urine",
        code: Some("9187-6"),
    },
    Term {
        id: KIND_FLUID_INTAKE,
        name: "fluid intake",
        alias: "intake",
        code: Some("9108-2"),
    },
    Term {
        id: KIND_PAIN_SEVERITY,
        name: "pain severity 0-10",
        alias: "pain",
        code: Some("72514-3"),
    },
];

pub const UNITS: &[Term] = &[
    Term {
        id: UNIT_MM_HG,
        name: "millimetre of mercury",
        alias: "mmHg",
        code: Some("mm[Hg]"),
    },
    Term {
        id: UNIT_PER_MINUTE,
        name: "per minute",
        alias: "/min",
        code: Some("/min"),
    },
    Term {
        id: UNIT_CELSIUS,
        name: "degree Celsius",
        alias: "Cel",
        code: Some("Cel"),
    },
    Term {
        id: UNIT_MILLILITER,
        name: "millilitre",
        alias: "mL",
        code: Some("mL"),
    },
    Term {
        id: UNIT_SCORE,
        name: "score",
        alias: "score",
        code: Some("{score}"),
    },
];

pub const METHODS: &[Term] = &[
    Term {
        id: METHOD_MEASURED_DEVICE,
        name: "measured-device",
        alias: "measured",
        code: None,
    },
    Term {
        id: METHOD_PHOTO_ESTIMATE,
        name: "photo-estimate",
        alias: "photo",
        code: None,
    },
    Term {
        id: METHOD_REPORTED_BY_SUBJECT,
        name: "reported-by-subject",
        alias: "reported",
        code: None,
    },
    Term {
        id: METHOD_ADMINISTERED,
        name: "administered",
        alias: "administered",
        code: None,
    },
];

/// Select one seed term by exact id, name, alias or code (case-insensitive).
pub fn select(terms: &'static [Term], input: &str, what: &str) -> Result<&'static Term> {
    let input = input.trim();
    let wanted = input.to_ascii_lowercase();
    let id = Id::from_hex(input);
    terms
        .iter()
        .find(|term| {
            Some(term.id) == id
                || term.name.eq_ignore_ascii_case(&wanted)
                || term.alias.eq_ignore_ascii_case(&wanted)
                || term
                    .code
                    .is_some_and(|code| code.eq_ignore_ascii_case(&wanted))
        })
        .ok_or_else(|| {
            let known: Vec<&str> = terms.iter().map(|term| term.alias).collect();
            anyhow!("unknown {what} {input:?}; known: {}", known.join(", "))
        })
}

/// The entity describing one seed term, with its name and code blobs.
pub fn term_fragment(term: &Term) -> Fragment {
    let (tag, code) = if KINDS.iter().any(|kind| kind.id == term.id) {
        (KIND_OBSERVATION_KIND, term.code.map(|code| (true, code)))
    } else if UNITS.iter().any(|unit| unit.id == term.id) {
        (KIND_UNIT, term.code.map(|code| (false, code)))
    } else {
        (KIND_METHOD, None)
    };
    let mut fragment = Fragment::empty();
    let name: TextHandle = fragment.put(term.name.to_owned());
    let code: Option<(bool, TextHandle)> =
        code.map(|(loinc, code)| (loinc, fragment.put(code.to_owned())));
    fragment += entity! { ExclusiveId::force_ref(&term.id) @
        metadata::tag: &tag,
        metadata::name: name,
        care::loinc?: code.filter(|(loinc, _)| *loinc).map(|(_, code)| code),
        care::ucum?: code.filter(|(loinc, _)| !*loinc).map(|(_, code)| code),
    };
    fragment
}

/// Every seed term, with the blobs that name them.
pub fn vocabulary() -> Fragment {
    let mut fragment = Fragment::empty();
    for term in KINDS.iter().chain(UNITS).chain(METHODS) {
        fragment += term_fragment(term);
    }
    fragment
}

/// The transient authoring input for one observation, after resolution.
pub struct Observation<'a> {
    pub subject: Id,
    pub kind: Id,
    pub value: f64,
    pub low: Option<f64>,
    pub high: Option<f64>,
    pub unit: Id,
    pub observed_at: IntervalValue,
    pub reported_at: IntervalValue,
    pub method: Id,
    pub photo: Option<PhotoHandle>,
    pub message: Option<Id>,
    pub note: Option<NoteHandle>,
    pub supersedes: &'a [Id],
}

/// Build one observation as a self-contained private archive: the intrinsic
/// observation entity plus the kind, unit and method entities it names.
/// Returns the observation id and the facts to seal.
pub fn observation_facts(input: &Observation<'_>) -> Result<(Id, TribleSet)> {
    if !input.value.is_finite()
        || input.low.is_some_and(|low| !low.is_finite())
        || input.high.is_some_and(|high| !high.is_finite())
    {
        bail!("values and bounds must be finite numbers");
    }
    if let (Some(low), Some(high)) = (input.low, input.high) {
        if low > high {
            bail!("lower bound {low} is above upper bound {high}");
        }
    }
    let record = entity! {
        metadata::tag: &KIND_OBSERVATION,
        care::subject: input.subject,
        care::kind: input.kind,
        care::value: input.value,
        care::low?: input.low,
        care::high?: input.high,
        care::unit: input.unit,
        care::observed_at: input.observed_at,
        care::reported_at: input.reported_at,
        care::method: input.method,
        care::photo?: input.photo,
        care::message?: input.message,
        care::note?: input.note,
        metadata::supersedes*: input.supersedes.iter().copied(),
    };
    let id = record.root().expect("observation has one intrinsic root");
    let mut facts = record.into_facts();
    for term in KINDS.iter().chain(UNITS).chain(METHODS) {
        if [input.kind, input.unit, input.method].contains(&term.id) {
            facts.union(term_fragment(term).into_facts());
        }
    }
    Ok((id, facts))
}

/// Read a vocabulary text annotation (name or code) from the seed blobs.
pub fn vocabulary_text(vocabulary: &Fragment, handle: TextHandle) -> Option<String> {
    use triblespace::core::repo::{BlobStoreGet, SnapshotSource};
    let mut store = vocabulary.blobs().clone();
    let reader = store.snapshot().ok()?;
    let text: View<str> = reader.get(handle).ok()?;
    Some(text.to_string())
}

/// Parse `YYYY-MM-DDTHH:MM[:SS][Z]` as UTC, or `A..B` as an interval.
pub fn parse_time(raw: &str) -> Result<IntervalValue> {
    let (from, to) = match raw.split_once("..") {
        Some((from, to)) => (parse_instant(from)?, parse_instant(to)?),
        None => {
            let at = parse_instant(raw)?;
            (at, at)
        }
    };
    if to < from {
        bail!("time interval {raw:?} ends before it starts");
    }
    (from, to)
        .try_to_inline()
        .map_err(|error| anyhow!("encode time {raw:?}: {error:?}"))
}

fn parse_instant(raw: &str) -> Result<Epoch> {
    let text = raw.trim().trim_end_matches('Z').replacen(' ', "T", 1);
    let invalid = || anyhow!("invalid time {raw:?}; expected YYYY-MM-DDTHH:MM[:SS] in UTC");
    let (date, time) = text.split_once('T').ok_or_else(invalid)?;
    let date: Vec<&str> = date.split('-').collect();
    let time: Vec<&str> = time.split(':').collect();
    if date.len() != 3 || !(2..=3).contains(&time.len()) {
        return Err(invalid());
    }
    let number = |s: &str| s.parse::<u32>().map_err(|_| invalid());
    let year = date[0].parse::<i32>().map_err(|_| invalid())?;
    let second = time.get(2).map(|s| number(s)).transpose()?.unwrap_or(0);
    Epoch::maybe_from_gregorian_utc(
        year,
        number(date[1])? as u8,
        number(date[2])? as u8,
        number(time[0])? as u8,
        number(time[1])? as u8,
        second as u8,
        0,
    )
    .map_err(|_| invalid())
}

/// Render an interval as UTC: one instant, or `A..B` when it is wide.
pub fn format_time(value: IntervalValue) -> String {
    let Ok((from, to)) = value.try_from_inline::<(Epoch, Epoch)>() else {
        return "?".to_owned();
    };
    if from == to {
        format_instant(from)
    } else {
        format!("{}..{}", format_instant(from), format_instant(to))
    }
}

fn format_instant(epoch: Epoch) -> String {
    let (y, mo, d, h, mi, s, _) = epoch.to_gregorian_utc();
    if s == 0 {
        format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}Z")
    } else {
        format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
    }
}

/// Format a number without a spurious fractional part.
pub fn format_number(value: f64) -> String {
    format!("{value}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(byte: u8) -> Id {
        Id::new([byte; 16]).unwrap()
    }

    #[test]
    fn published_care_ids_remain_byte_exact() {
        // Attribute ids derive from their minted anchor and encoding; the
        // kind markers are the minted values themselves.
        assert_eq!(
            KIND_OBSERVATION,
            Id::from_hex("88799AE91CED5F960D0E3D6B4026C76E").unwrap()
        );
        assert_eq!(
            KIND_OBSERVATION_RECORD,
            Id::from_hex("48EBBA4464EA22DF79B9B1A955CCAF02").unwrap()
        );
        assert_ne!(
            care::subject.id(),
            crate::schemas::relations::relations::subject.id()
        );
    }

    #[test]
    fn observation_is_one_self_contained_intrinsic_archive() {
        let at = parse_time("2026-10-30T21:29").unwrap();
        let input = Observation {
            subject: id(1),
            kind: KIND_SYSTOLIC_BP,
            value: 122.0,
            low: None,
            high: None,
            unit: UNIT_MM_HG,
            observed_at: at,
            reported_at: at,
            method: METHOD_MEASURED_DEVICE,
            photo: None,
            message: None,
            note: None,
            supersedes: &[],
        };
        let (first, facts) = observation_facts(&input).unwrap();
        let (second, again) = observation_facts(&input).unwrap();
        assert_eq!(first, second);
        assert_eq!(facts, again);
        // The kind, unit and method entities travel with the observation.
        for term in [KIND_SYSTOLIC_BP, UNIT_MM_HG, METHOD_MEASURED_DEVICE] {
            assert!(exists!(
                pattern!(&facts, [{ term @ metadata::name: _?name }])
            ));
        }
        let bounded = Observation {
            low: Some(130.0),
            high: Some(120.0),
            ..input
        };
        assert!(observation_facts(&bounded).is_err());
    }

    #[test]
    fn times_parse_as_utc_points_and_intervals() {
        let point = parse_time("2026-10-30T21:29Z").unwrap();
        assert_eq!(format_time(point), "2026-10-30T21:29Z");
        let wide = parse_time("2026-10-30T21:00..2026-10-30T21:30").unwrap();
        assert_eq!(format_time(wide), "2026-10-30T21:00Z..2026-10-30T21:30Z");
        assert!(parse_time("2026-10-30T22:00..2026-10-30T21:00").is_err());
        assert!(parse_time("yesterday").is_err());
    }

    #[test]
    fn terms_select_by_alias_name_code_and_id() {
        assert_eq!(
            select(KINDS, "8480-6", "kind").unwrap().id,
            KIND_SYSTOLIC_BP
        );
        assert_eq!(
            select(KINDS, "Pain", "kind").unwrap().id,
            KIND_PAIN_SEVERITY
        );
        assert_eq!(select(UNITS, "mm[Hg]", "unit").unwrap().id, UNIT_MM_HG);
        assert_eq!(
            select(METHODS, &format!("{METHOD_ADMINISTERED:x}"), "method")
                .unwrap()
                .id,
            METHOD_ADMINISTERED
        );
        assert!(select(KINDS, "glucose", "kind").is_err());
        let vocabulary = vocabulary();
        let code = find!(code: TextHandle,
            pattern!(vocabulary.facts(), [{ KIND_HEART_RATE @ care::loinc: ?code }]))
        .next()
        .unwrap();
        assert_eq!(vocabulary_text(&vocabulary, code).unwrap(), "8867-4");
    }
}
