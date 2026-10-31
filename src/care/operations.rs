//! Configured Care operations: seal observations as Secrets records, open
//! what this key can read, query the opened relation at the point of use, and
//! share a subject's readings through Secrets group grants.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use ed25519_dalek::{SigningKey, VerifyingKey};
use triblespace::core::blob::encodings::succinctarchive::{
    Rank9AcceleratedSuccinctArchiveBlob, SuccinctArchiveBlob,
};
use triblespace::core::collection::{CollectionHandle, CollectionStoreExt};
use triblespace::core::metadata;
use triblespace::core::repo::SnapshotSource;
use triblespace::prelude::*;

use super::{
    format_time, observation_facts, select, IntervalValue, NoteHandle, Observation, PhotoHandle,
    TextHandle, GROUP_LABEL, KINDS, METHODS, RECORD_LABEL, UNITS,
};
use crate::clock;
use crate::relations::{self, SelectorOutcome};
use crate::schemas::care::{care as attrs, KIND_OBSERVATION, KIND_OBSERVATION_RECORD};
use crate::schemas::relations::DEFAULT_SCOPE_ID as RELATIONS_SCOPE_ID;
use crate::secrets::record::{self, Delivery, OpenedRecord};
use crate::secrets::resource::{self, DeliveryLimits, SecretTarget};
use crate::storage::{AcquiringReader, FacultySnapshot, FacultyStore, Storage};

type Reader = AcquiringReader<FacultySnapshot>;

#[derive(Clone, Debug)]
pub struct Care {
    storage: Storage,
}

/// Literal authoring input from a frontend. Selectors are resolved here.
#[derive(Clone, Debug, Default)]
pub struct AddInput {
    pub subject: String,
    pub kind: String,
    pub value: f64,
    pub low: Option<f64>,
    pub high: Option<f64>,
    pub unit: String,
    pub observed: String,
    /// Defaults to now when absent.
    pub reported: Option<String>,
    pub method: String,
    pub photo: Option<Vec<u8>>,
    pub message: Option<String>,
    pub note: Option<String>,
    pub supersedes: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Added {
    /// The intrinsic observation id (what `--supersedes` names).
    pub observation: Id,
    /// The Secrets record carrying the sealed observation.
    pub record: Id,
    pub subject: Id,
    /// The Secrets group resource the record is delivered under.
    pub group: CollectionHandle,
}

/// One displayed observation: the newest correction of its line.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub observation: Id,
    pub observed_at: IntervalValue,
    pub reported_at: IntervalValue,
    pub kind: String,
    pub loinc: String,
    pub value: f64,
    pub low: Option<f64>,
    pub high: Option<f64>,
    pub unit: String,
    pub method: String,
    /// Ciphertext handle of the sealed photo, hex.
    pub photo: Option<String>,
    pub message: Option<Id>,
    pub note: Option<String>,
    /// Observations this one corrects.
    pub supersedes: Vec<Id>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Table {
    pub subject: Id,
    pub rows: Vec<Row>,
    /// Records present that this key cannot open.
    pub unreadable: usize,
}

/// Receipt of one `care grant`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Granted {
    pub subject: Id,
    pub recipient: VerifyingKey,
    /// The Secrets group resources granted, one per kind.
    pub groups: Vec<CollectionHandle>,
    /// Envelopes delivered by the maintenance pass that followed.
    pub envelopes: usize,
}

/// Every care record this key can open, from one frozen Secrets view.
struct Opened {
    reader: Reader,
    records: Vec<OpenedRecord<Reader>>,
    facts: TribleSet,
    unreadable: usize,
}

impl Opened {
    fn read(
        storage: &Storage,
        pile: &mut FacultyStore,
        signer: &SigningKey,
        runtime: &Arc<tokio::runtime::Runtime>,
    ) -> Result<Self> {
        let collection =
            storage.open_secrets_collection(pile, signer.verifying_key(), runtime, false)?;
        let snapshot =
            crate::secrets::acquiring_snapshot(pile, collection, signer, runtime, false)?;
        let reader = snapshot.store_snapshot().clone();
        let mut opened = Self {
            reader: reader.clone(),
            records: Vec::new(),
            facts: TribleSet::new(),
            unreadable: 0,
        };
        let Some(public) = snapshot.facts() else {
            return Ok(opened);
        };
        let ids: BTreeSet<Id> = find!(id: Id,
            pattern!(public, [{ ?id @ metadata::tag: &KIND_OBSERVATION_RECORD }]))
        .collect();
        for id in ids {
            match record::open(reader.clone(), public, id, signer) {
                Ok(records) => {
                    for opened_record in records {
                        opened.facts.union(opened_record.facts().clone());
                        opened.records.push(opened_record);
                    }
                }
                Err(_) => opened.unreadable += 1,
            }
        }
        Ok(opened)
    }

    /// Group resources delivering this subject's readings (of one kind).
    fn groups(&self, subject: Id, kind: Option<Id>) -> BTreeSet<CollectionHandle> {
        self.records
            .iter()
            .filter(|opened| {
                find!(kind_id: Id, pattern!(opened.facts(), [{
                    metadata::tag: &KIND_OBSERVATION,
                    attrs::subject: &subject,
                    attrs::kind: ?kind_id,
                }]))
                .any(|kind_id| kind.is_none_or(|want| want == kind_id))
            })
            .filter_map(|opened| opened.resource())
            .filter(|handle| resource::is_group(&self.reader, *handle))
            .collect()
    }
}

impl Care {
    pub fn new(pile: PathBuf, key: Option<PathBuf>) -> Self {
        Self::with_storage(Storage::new(pile, key))
    }
    pub fn with_storage(storage: Storage) -> Self {
        Self { storage }
    }

    /// Seal one observation with its note and photo as one Secrets record,
    /// delivered under the subject's group for this kind (created on first use).
    pub fn add(&self, input: &AddInput) -> Result<Added> {
        let kind = select(KINDS, &input.kind, "kind")?.id;
        let unit = select(UNITS, &input.unit, "unit")?.id;
        let method = select(METHODS, &input.method, "method")?.id;
        let observed_at = super::parse_time(&input.observed).context("--observed")?;
        let reported_at = match &input.reported {
            Some(raw) => super::parse_time(raw).context("--reported")?,
            None => clock::point_now()?,
        };
        let message = input
            .message
            .as_deref()
            .map(|raw| exact_id(raw, "message anchor"))
            .transpose()?;
        let supersedes = input
            .supersedes
            .iter()
            .map(|raw| exact_id(raw, "superseded observation"))
            .collect::<Result<Vec<_>>>()?;
        let note = input
            .note
            .as_deref()
            .map(str::trim)
            .filter(|note| !note.is_empty());
        self.storage.with_store(|pile, signer, runtime| {
            let subject = resolve_subject(&self.storage, pile, signer, runtime, &input.subject)?;
            let sealed_at = clock::point_now()?;
            let mut public = Fragment::empty();
            let existing = Opened::read(&self.storage, pile, signer, runtime)?
                .groups(subject, Some(kind))
                .into_iter()
                .next();
            let group = match existing {
                Some(group) => group,
                None => {
                    let created = resource::create_group(signer, GROUP_LABEL, sealed_at)?;
                    public += created.fragment;
                    created.resource
                }
            };
            let mut observation = None;
            let sealed = record::seal(
                signer,
                RECORD_LABEL,
                sealed_at,
                Delivery::Group(group),
                |sealer| {
                    let note: Option<NoteHandle> = note
                        .map(|text| {
                            sealer.seal_blob::<blobencodings::UTF8String>(
                                text.to_owned().to_blob(),
                                &[],
                            )
                        })
                        .transpose()?;
                    let photo: Option<PhotoHandle> = input
                        .photo
                        .as_ref()
                        .map(|bytes| {
                            sealer
                                .seal_blob::<blobencodings::RawBytes>(bytes.clone().to_blob(), &[])
                        })
                        .transpose()?;
                    let (id, facts) = observation_facts(&Observation {
                        subject,
                        kind,
                        value: input.value,
                        low: input.low,
                        high: input.high,
                        unit,
                        observed_at,
                        reported_at,
                        method,
                        photo,
                        message,
                        note,
                        supersedes: &supersedes,
                    })?;
                    observation = Some(id);
                    Ok(facts)
                },
            )?;
            let record = sealed.secret;
            public += sealed.fragment;
            public += entity! { ExclusiveId::force_ref(&record) @
                metadata::tag: &KIND_OBSERVATION_RECORD,
            };
            let collection = self.storage.open_secrets_collection(
                pile,
                signer.verifying_key(),
                runtime,
                true,
            )?;
            pile.commit(collection.source(), signer, public)
                .context("publish sealed care observation")?;
            drop(
                runtime
                    .block_on(crate::storage::ensure_downstream(
                        pile,
                        collection.source(),
                        signer,
                    ))
                    .context("Care observation committed, but ensuring its views failed")?,
            );
            Ok(Added {
                observation: observation.expect("the builder ran"),
                record,
                subject,
                group,
            })
        })
    }

    /// Share a subject's readings (of one kind) with another key: one Secrets
    /// grant per group, then one Secrets delivery pass. Later readings in the
    /// same groups reach the grantee at the next delivery maintenance.
    pub fn grant(&self, subject: &str, to: &str, kind: Option<&str>) -> Result<Granted> {
        let recipient = parse_recipient(to)?;
        let kind = kind
            .map(|raw| select(KINDS, raw, "kind"))
            .transpose()?
            .map(|term| term.id);
        let (subject, groups) = self.storage.with_store(|pile, signer, runtime| {
            let subject = resolve_subject(&self.storage, pile, signer, runtime, subject)?;
            let groups = Opened::read(&self.storage, pile, signer, runtime)?.groups(subject, kind);
            Ok((subject, groups))
        })?;
        if groups.is_empty() {
            bail!("no readable care readings to share for this subject and kind");
        }
        let secrets = crate::secrets::Secrets::with_storage(self.storage.clone());
        let targets: Vec<SecretTarget> =
            groups.iter().copied().map(SecretTarget::Resource).collect();
        for target in &targets {
            secrets.grant(*target, recipient, DeliveryLimits::default(), false)?;
        }
        let envelopes = secrets.maintain_selected(&targets)?;
        Ok(Granted {
            subject,
            recipient,
            groups: groups.into_iter().collect(),
            envelopes,
        })
    }

    /// Every readable observation of one subject, newest correction per line.
    pub fn table(&self, subject: &str, since: Option<&str>, kind: Option<&str>) -> Result<Table> {
        let since = since
            .map(|raw| super::parse_time(raw).context("--since"))
            .transpose()?
            .map(|value| {
                value
                    .try_from_inline::<(i128, i128)>()
                    .map(|(lower, _)| lower)
                    .unwrap_or(i128::MIN)
            });
        let kind = kind
            .map(|raw| select(KINDS, raw, "kind"))
            .transpose()?
            .map(|term| term.id);
        self.storage.with_store(|pile, signer, runtime| {
            let subject = resolve_subject(&self.storage, pile, signer, runtime, subject)?;
            let opened = Opened::read(&self.storage, pile, signer, runtime)?;
            let facts = &opened.facts;
            let vocabulary = super::vocabulary();
            let text = |handle: TextHandle| {
                super::vocabulary_text(&vocabulary, handle).unwrap_or_else(|| "?".to_owned())
            };
            let superseded: BTreeSet<Id> = find!((newer: Id, older: Id),
            pattern!(facts, [{ ?newer @
                metadata::tag: &KIND_OBSERVATION,
                metadata::supersedes: ?older,
            }]))
            .map(|(_, older)| older)
            .collect();
            let mut rows = Vec::new();
            for (observation, kind_id, value, unit, observed_at, reported_at, method) in find!(
                (observation: Id, kind_id: Id, value: Inline<inlineencodings::F64>, unit: Id,
                 observed_at: IntervalValue, reported_at: IntervalValue, method: Id),
                pattern!(facts, [{ ?observation @
                    metadata::tag: &KIND_OBSERVATION,
                    attrs::subject: &subject,
                    attrs::kind: ?kind_id,
                    attrs::value: ?value,
                    attrs::unit: ?unit,
                    attrs::observed_at: ?observed_at,
                    attrs::reported_at: ?reported_at,
                    attrs::method: ?method,
                }])
            ) {
                if superseded.contains(&observation) || kind.is_some_and(|want| want != kind_id) {
                    continue;
                }
                let (lower, upper): (i128, i128) = observed_at
                    .try_from_inline()
                    .unwrap_or((i128::MIN, i128::MIN));
                if since.is_some_and(|since| upper < since) {
                    continue;
                }
                let name_of = |entity: Id| {
                    find!(name: TextHandle, pattern!(facts, [{ entity @ metadata::name: ?name }]))
                        .next()
                        .map(&text)
                        .unwrap_or_else(|| format!("{entity:x}"))
                };
                let number = |found: Option<Inline<inlineencodings::F64>>| {
                    found.map(|value| value.try_from_inline::<f64>().unwrap())
                };
                let note = find!(handle: NoteHandle,
                    pattern!(facts, [{ observation @ attrs::note: ?handle }]))
                .next()
                .map(|handle| {
                    opened
                        .records
                        .iter()
                        .find(|record| {
                            exists!(
                                pattern!(record.facts(), [{ observation @ attrs::note: handle }])
                            )
                        })
                        .and_then(|record| record.open_child(handle).ok())
                        .and_then(|blob| String::from_utf8(blob.bytes.as_ref().to_vec()).ok())
                        .unwrap_or_else(|| "[note not readable]".to_owned())
                });
                rows.push((
                    (lower, upper, observation),
                    Row {
                        observation,
                        observed_at,
                        reported_at,
                        kind: name_of(kind_id),
                        loinc: find!(code: TextHandle,
                            pattern!(facts, [{ kind_id @ attrs::loinc: ?code }]))
                        .next()
                        .map(&text)
                        .unwrap_or_default(),
                        value: value.try_from_inline::<f64>().unwrap(),
                        low: number(
                            find!(v: Inline<inlineencodings::F64>,
                                pattern!(facts, [{ observation @ attrs::low: ?v }]))
                            .next(),
                        ),
                        high: number(
                            find!(v: Inline<inlineencodings::F64>,
                                pattern!(facts, [{ observation @ attrs::high: ?v }]))
                            .next(),
                        ),
                        unit: find!(code: TextHandle,
                            pattern!(facts, [{ unit @ attrs::ucum: ?code }]))
                        .next()
                        .map(&text)
                        .unwrap_or_else(|| name_of(unit)),
                        method: name_of(method),
                        photo: find!(photo: PhotoHandle,
                            pattern!(facts, [{ observation @ attrs::photo: ?photo }]))
                        .next()
                        .map(|photo| hex::encode(photo.raw)),
                        message: find!(message: Id,
                            pattern!(facts, [{ observation @ attrs::message: ?message }]))
                        .next(),
                        note,
                        supersedes: find!(older: Id,
                            pattern!(facts, [{ observation @ metadata::supersedes: ?older }]))
                        .collect(),
                    },
                ));
            }
            rows.sort_by(|left, right| left.0.cmp(&right.0));
            Ok(Table {
                subject,
                rows: rows.into_iter().map(|(_, row)| row).collect(),
                unreadable: opened.unreadable,
            })
        })
    }
}

fn exact_id(raw: &str, what: &str) -> Result<Id> {
    match Id::from_hex(raw.trim()) {
        Some(id) => Ok(id),
        None => bail!("{what} must be an exact 32-character id, got {raw:?}"),
    }
}

/// An Ed25519 public key as 64 hex characters.
fn parse_recipient(raw: &str) -> Result<VerifyingKey> {
    let bytes = hex::decode(raw.trim())
        .ok()
        .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok());
    let Some(bytes) = bytes else {
        bail!("--to must be an Ed25519 public key as 64 hex characters");
    };
    VerifyingKey::from_bytes(&bytes).context("--to is not a valid Ed25519 public key")
}

fn resolve_subject(
    storage: &Storage,
    pile: &mut FacultyStore,
    signer: &SigningKey,
    runtime: &Arc<tokio::runtime::Runtime>,
    selector: &str,
) -> Result<Id> {
    let (snapshot, facts) = observe_relations(storage, pile, signer, runtime)?;
    match relations::resolve_person(&snapshot, &facts, selector, true)? {
        SelectorOutcome::Unique(id) => Ok(id),
        outcome => outcome.require_unique("person", selector),
    }
}

/// The maintained Relations relation, read only to resolve the subject.
fn observe_relations(
    storage: &Storage,
    pile: &mut FacultyStore,
    signer: &SigningKey,
    runtime: &Arc<tokio::runtime::Runtime>,
) -> Result<(Reader, crate::storage::FactArchive)> {
    let source =
        storage.open_collection_read(pile, RELATIONS_SCOPE_ID, signer.verifying_key(), runtime)?;
    let succinct = pile.attach::<SuccinctArchiveBlob>(source, ())?;
    let rank9 = pile.attach::<Rank9AcceleratedSuccinctArchiveBlob>(source, succinct)?;
    {
        let mut local = pile.store();
        pollster::block_on(async {
            crate::storage::tolerate_own_lag(local.maintain_attached(succinct, signer).await)?;
            crate::storage::tolerate_own_lag(local.maintain_attached(rank9, signer).await)
        })
        .context("maintain Relations fact collection")?;
    }
    let snapshot = pile.snapshot().context("freeze Relations snapshot")?;
    let snapshot = AcquiringReader::new(snapshot, Arc::clone(runtime));
    let facts = crate::storage::acquire_facts(&snapshot, rank9)
        .context("read Relations Rank9 collection")?;
    Ok((snapshot, facts))
}

/// Exactly what a doctor's CSV holds; display only, no interpretation.
pub fn csv(table: &Table) -> String {
    fn field(text: &str) -> String {
        if text.contains([',', '"', '\n', '\r']) {
            format!("\"{}\"", text.replace('"', "\"\""))
        } else {
            text.to_owned()
        }
    }
    let mut out = String::from(
        "observation,observed_at,reported_at,kind,loinc,value,low,high,unit,method,photo_sealed,message,supersedes,note\n",
    );
    for row in &table.rows {
        let number = |value: Option<f64>| value.map(super::format_number).unwrap_or_default();
        let fields = [
            format!("{:x}", row.observation),
            format_time(row.observed_at),
            format_time(row.reported_at),
            row.kind.clone(),
            row.loinc.clone(),
            super::format_number(row.value),
            number(row.low),
            number(row.high),
            row.unit.clone(),
            row.method.clone(),
            row.photo.clone().unwrap_or_default(),
            row.message.map(|id| format!("{id:x}")).unwrap_or_default(),
            row.supersedes
                .iter()
                .map(|id| format!("{id:x}"))
                .collect::<Vec<_>>()
                .join(" "),
            row.note.clone().unwrap_or_default(),
        ];
        out.push_str(
            &fields
                .iter()
                .map(|f| field(f))
                .collect::<Vec<_>>()
                .join(","),
        );
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relations::{ProfileInput, Relations};

    /// The care-level second-key check: after `care grant`, a different key
    /// opens the granted subject's records through the Secrets record path,
    /// and a record in another subject's group stays closed to it.
    #[test]
    fn grant_lets_a_second_key_open_exactly_the_granted_groups() {
        let directory = tempfile::tempdir().unwrap();
        let pile = directory.path().join("care.pile");
        let key = directory.path().join("care.key");
        std::fs::File::create(&pile).unwrap();
        crate::storage::initialize_signer(&pile, Some(&key)).unwrap();
        let relations = Relations::new(pile.clone(), Some(key.clone()));
        let person = |label: &str| {
            let added = relations
                .add(
                    ProfileInput {
                        label: label.into(),
                        ..Default::default()
                    },
                    None,
                    &[],
                )
                .unwrap();
            format!("{:x}", added.person)
        };
        let (shared, other) = (person("Shared"), person("Other"));
        let care = Care::new(pile.clone(), Some(key.clone()));
        let reading = |subject: &str, kind: &str, value: f64, unit: &str| AddInput {
            subject: subject.into(),
            kind: kind.into(),
            value,
            unit: unit.into(),
            observed: "2026-10-30T21:29".into(),
            reported: Some("2026-10-30T21:31".into()),
            method: "measured-device".into(),
            note: Some(format!("note {value}")),
            ..Default::default()
        };
        let first = care
            .add(&reading(&shared, "systolic", 120.0, "mm[Hg]"))
            .unwrap();
        let second = care
            .add(&reading(&shared, "systolic", 118.0, "mm[Hg]"))
            .unwrap();
        let pulse = care
            .add(&reading(&shared, "heart-rate", 70.0, "/min"))
            .unwrap();
        let elsewhere = care
            .add(&reading(&other, "systolic", 130.0, "mm[Hg]"))
            .unwrap();
        assert_eq!(first.group, second.group, "one group per subject and kind");
        assert_ne!(first.group, pulse.group);
        assert_ne!(first.group, elsewhere.group);

        let bob = SigningKey::from_bytes(&[77; 32]);
        let to = hex::encode(bob.verifying_key().to_bytes());
        let granted = care.grant(&shared, &to, Some("systolic")).unwrap();
        assert_eq!(granted.groups, vec![first.group]);
        assert_eq!(granted.envelopes, 2);
        // A later reading in the granted group reaches bob at the next pass.
        let later = care
            .add(&reading(&shared, "systolic", 116.0, "mm[Hg]"))
            .unwrap();
        let again = care.grant(&shared, &to, Some("systolic")).unwrap();
        assert_eq!(again.envelopes, 1);

        let storage = Storage::new(pile, Some(key));
        storage
            .with_store(|store, signer, runtime| {
                let collection = storage.open_secrets_collection(
                    store,
                    signer.verifying_key(),
                    runtime,
                    false,
                )?;
                let snapshot =
                    crate::secrets::acquiring_snapshot(store, collection, signer, runtime, false)?;
                let public = snapshot.facts().unwrap();
                let reader = snapshot.store_snapshot().clone();
                for added in [first, second, later] {
                    let opened = record::open(reader.clone(), public, added.record, &bob)?;
                    let note = find!(note: NoteHandle,
                        pattern!(opened[0].facts(), [{ attrs::note: ?note }]))
                    .next()
                    .unwrap();
                    assert!(opened[0].open_child(note).is_ok());
                }
                for closed in [pulse, elsewhere] {
                    assert!(record::open(reader.clone(), public, closed.record, &bob).is_err());
                }
                Ok(())
            })
            .unwrap();
        assert!(care.grant(&other, "not-a-key", None).is_err());
    }
}
