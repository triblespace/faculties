//! Configured Care operations: seal observations through Secrets, open what
//! this key can read, and query the opened relation at the point of use.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use ed25519_dalek::SigningKey;
use triblespace::core::blob::encodings::simplearchive::SimpleArchive;
use triblespace::core::blob::encodings::succinctarchive::{
    Rank9AcceleratedSuccinctArchiveBlob, SuccinctArchiveBlob,
};
use triblespace::core::blob::{Blob, Bytes};
use triblespace::core::collection::CollectionStoreExt;
use triblespace::core::metadata;
use triblespace::core::repo::SnapshotSource;
use triblespace::prelude::*;

use super::{
    format_time, observation_facts, select, IntervalValue, Observation, TextHandle,
    ATTACHMENT_LABEL, KINDS, METHODS, RECORD_LABEL, UNITS,
};
use crate::clock;
use crate::relations::{self, SelectorOutcome};
use crate::schemas::care::{care as attrs, KIND_OBSERVATION, KIND_OBSERVATION_RECORD};
use crate::schemas::relations::DEFAULT_SCOPE_ID as RELATIONS_SCOPE_ID;
use crate::secrets::resource;
use crate::storage::{AcquiringReader, FacultyStore, Storage};

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
    /// The Secrets version carrying the sealed observation.
    pub record: Id,
    pub subject: Id,
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

impl Care {
    pub fn new(pile: PathBuf, key: Option<PathBuf>) -> Self {
        Self::with_storage(Storage::new(pile, key))
    }
    pub fn with_storage(storage: Storage) -> Self {
        Self { storage }
    }

    /// Seal one observation (and its note/photo) into one Secrets commit.
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
            let subject = {
                let (snapshot, facts) = observe_relations(&self.storage, pile, signer, runtime)?;
                match relations::resolve_person(&snapshot, &facts, &input.subject, true)? {
                    SelectorOutcome::Unique(id) => id,
                    outcome => outcome.require_unique("person", &input.subject)?,
                }
            };
            let sealed_at = clock::point_now()?;
            let mut public = Fragment::empty();
            let note = note
                .map(|text| -> Result<(TextHandle, Id)> {
                    let sealed = resource::seal_version(
                        signer,
                        ATTACHMENT_LABEL,
                        text.as_bytes(),
                        sealed_at,
                    )?;
                    let blob: Blob<blobencodings::UTF8String> = text.to_owned().to_blob();
                    let handle = blob.get_handle();
                    public += sealed.fragment;
                    Ok((handle, sealed.secret))
                })
                .transpose()?;
            let photo = input
                .photo
                .as_deref()
                .map(|bytes| -> Result<_> {
                    let sealed =
                        resource::seal_version(signer, ATTACHMENT_LABEL, bytes, sealed_at)?;
                    let handle = Blob::<blobencodings::RawBytes>::new(Bytes::from(bytes.to_vec()))
                        .get_handle();
                    public += sealed.fragment;
                    Ok((handle, sealed.secret))
                })
                .transpose()?;
            let (observation, facts) = observation_facts(&Observation {
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
            let archive: Blob<SimpleArchive> = facts.to_blob();
            let sealed =
                resource::seal_version(signer, RECORD_LABEL, archive.bytes.as_ref(), sealed_at)?;
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
                observation,
                record,
                subject,
            })
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
            let subject = {
                let (snapshot, facts) = observe_relations(&self.storage, pile, signer, runtime)?;
                match relations::resolve_person(&snapshot, &facts, subject, true)? {
                    SelectorOutcome::Unique(id) => id,
                    outcome => outcome.require_unique("person", subject)?,
                }
            };
            let collection = self.storage.open_secrets_collection(
                pile,
                signer.verifying_key(),
                runtime,
                false,
            )?;
            let snapshot =
                crate::secrets::acquiring_snapshot(pile, collection, signer, runtime, false)?;
            let mut opened = TribleSet::new();
            let mut unreadable = 0;
            if let Some(public) = snapshot.facts() {
                let records: BTreeSet<Id> = find!(record: Id,
                    pattern!(public, [{ ?record @ metadata::tag: &KIND_OBSERVATION_RECORD }]))
                .collect();
                for record in records {
                    let archive = snapshot.open(record, signer).and_then(|bytes| {
                        Blob::<SimpleArchive>::new(Bytes::from(bytes))
                            .try_from_blob::<TribleSet>()
                            .map_err(|_| anyhow::anyhow!("sealed record is not an archive"))
                    });
                    match archive {
                        Ok(facts) => opened.union(facts),
                        Err(_) => unreadable += 1,
                    }
                }
            }
            let vocabulary = super::vocabulary();
            let text = |handle: TextHandle| {
                super::vocabulary_text(&vocabulary, handle).unwrap_or_else(|| "?".to_owned())
            };
            let superseded: BTreeSet<Id> = find!((newer: Id, older: Id),
            pattern!(&opened, [{ ?newer @
                metadata::tag: &KIND_OBSERVATION,
                metadata::supersedes: ?older,
            }]))
            .map(|(_, older)| older)
            .collect();
            let mut rows = Vec::new();
            for (observation, kind_id, value, unit, observed_at, reported_at, method) in find!(
                (observation: Id, kind_id: Id, value: Inline<inlineencodings::F64>, unit: Id,
                 observed_at: IntervalValue, reported_at: IntervalValue, method: Id),
                pattern!(&opened, [{ ?observation @
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
                    find!(name: TextHandle,
                        pattern!(&opened, [{ entity @ metadata::name: ?name }]))
                    .next()
                    .map(&text)
                    .unwrap_or_else(|| format!("{entity:x}"))
                };
                let number = |attribute: Option<Inline<inlineencodings::F64>>| {
                    attribute.map(|value| value.try_from_inline::<f64>().unwrap())
                };
                let note = find!((handle: TextHandle, secret: Id),
                    pattern!(&opened, [{ observation @
                        attrs::note: ?handle, attrs::note_secret: ?secret }]))
                .next()
                .map(|(handle, secret)| {
                    snapshot
                        .open(secret, signer)
                        .ok()
                        .and_then(|bytes| String::from_utf8(bytes).ok())
                        .filter(|text| {
                            let blob: Blob<blobencodings::UTF8String> = text.clone().to_blob();
                            blob.get_handle() == handle
                        })
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
                            pattern!(&opened, [{ kind_id @ attrs::loinc: ?code }]))
                        .next()
                        .map(&text)
                        .unwrap_or_default(),
                        value: value.try_from_inline::<f64>().unwrap(),
                        low: number(
                            find!(v: Inline<inlineencodings::F64>,
                                pattern!(&opened, [{ observation @ attrs::low: ?v }]))
                            .next(),
                        ),
                        high: number(
                            find!(v: Inline<inlineencodings::F64>,
                                pattern!(&opened, [{ observation @ attrs::high: ?v }]))
                            .next(),
                        ),
                        unit: find!(code: TextHandle,
                            pattern!(&opened, [{ unit @ attrs::ucum: ?code }]))
                        .next()
                        .map(&text)
                        .unwrap_or_else(|| name_of(unit)),
                        method: name_of(method),
                        photo: find!(photo: super::BytesHandle,
                            pattern!(&opened, [{ observation @ attrs::photo: ?photo }]))
                        .next()
                        .map(|photo| hex::encode(photo.raw)),
                        message: find!(message: Id,
                            pattern!(&opened, [{ observation @ attrs::message: ?message }]))
                        .next(),
                        note,
                        supersedes: find!(older: Id,
                            pattern!(&opened, [{ observation @ metadata::supersedes: ?older }]))
                        .collect(),
                    },
                ));
            }
            rows.sort_by(|left, right| left.0.cmp(&right.0));
            Ok(Table {
                subject,
                rows: rows.into_iter().map(|(_, row)| row).collect(),
                unreadable,
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

/// The maintained Relations relation, read only to resolve the subject.
fn observe_relations(
    storage: &Storage,
    pile: &mut FacultyStore,
    signer: &SigningKey,
    runtime: &Arc<tokio::runtime::Runtime>,
) -> Result<(
    AcquiringReader<crate::storage::FacultySnapshot>,
    crate::storage::FactArchive,
)> {
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
        "observation,observed_at,reported_at,kind,loinc,value,low,high,unit,method,photo_blake3,message,supersedes,note\n",
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
