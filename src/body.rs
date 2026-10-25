//! Canonical Body records and maintained collection views.
//!
//! Body is an immutable log of deliberate captures and sparse VLA intents.
//! Writers construct intrinsically identified records. Ordinary readers query
//! the decodable rows they need from a shard-preserving Succinct view.

use crate::storage::FactRead;

pub mod cli;
pub mod device;
pub mod mcp;
mod operations;
pub mod presentation;
pub use operations::*;

use anyhow::{anyhow, bail, Context, Result};
use ed25519_dalek::{SigningKey, VerifyingKey};
use triblespace::core::blob::encodings::succinctarchive::{
    Rank9AcceleratedSuccinctArchiveBlob, SuccinctArchiveBlob,
};
use triblespace::core::collection::lww_register::{LwwIndex, LwwQuery, LwwRegisterBlob};
use triblespace::core::collection::{CollectionRead, CollectionSnapshotExt, CollectionStoreExt};
use triblespace::core::metadata;
use triblespace::core::query::TriblePattern;
use triblespace::core::repo::pile::{Pile, PileSnapshot};
use triblespace::core::repo::{BlobStoreGet, CapabilityProofRead, SnapshotSource};
use triblespace::prelude::*;

use crate::collection_names::open_configured;
use crate::schemas::body::{capture, intent, DEFAULT_SCOPE_ID, KIND_CAPTURE, KIND_INTENT};
use crate::storage::FactArchive;

pub type IntervalValue = Inline<inlineencodings::NsTAIInterval>;
pub type RawHandle = Inline<inlineencodings::Handle<blobencodings::RawBytes>>;
pub type TextHandle = Inline<inlineencodings::Handle<blobencodings::UTF8String>>;
pub type DimensionValue = Inline<inlineencodings::U256BE>;

/// One exact deliberate capture projected from the Body collection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CaptureRow {
    pub id: Id,
    pub created_at: IntervalValue,
    pub frame: Option<RawHandle>,
    pub mime: Option<TextHandle>,
    pub width: Option<DimensionValue>,
    pub height: Option<DimensionValue>,
    pub modality: TextHandle,
    pub note: Option<TextHandle>,
    pub pose: TextHandle,
}

/// One exact sparse VLA intent projected from the Body collection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IntentRow {
    pub id: Id,
    pub created_at: IntervalValue,
    pub text: TextHandle,
}

/// One coherent Body fact view plus its maintained intent register.
///
/// Facts, known intent winners, and the attachment reader come from one
/// immutable post-maintenance store snapshot. Each target contributes its
/// resident view; their supports need not be identical.
pub struct BodySnapshot<R = PileSnapshot> {
    facts: FactArchive,
    store_snapshot: R,
    intents: LwwQuery,
}

impl<R> BodySnapshot<R> {
    /// Shard-preserving facts from the resident target collection.
    pub fn facts(&self) -> &FactArchive {
        &self.facts
    }

    /// Store snapshot shared by the selected fact and intent views.
    pub fn store_snapshot(&self) -> &R {
        &self.store_snapshot
    }

    /// Maintained order for the Body intent register.
    pub fn intent_register(&self) -> &LwwQuery {
        &self.intents
    }

    /// Consume the coherent snapshot into facts, store snapshot, and index.
    pub fn into_parts(self) -> (FactArchive, R, LwwQuery) {
        (self.facts, self.store_snapshot, self.intents)
    }
}

/// Exact maintained tag-order projection used to select the current intent.
///
/// `metadata::tag` is the identity coordinate: every intent event states the
/// single register value [`KIND_INTENT`]. `metadata::created_at` is an
/// order-preserving point interval, and [`LwwQuery`] breaks equal-time ties by
/// intrinsic event id, matching the historical JIT reader exactly. Capture
/// rows form an independent `KIND_CAPTURE` register in the same target bytes;
/// [`latest_intent`] scopes the read with `winner(KIND_INTENT)`.
pub fn intent_register_collection<S>(
    store: &mut S,
    authority: VerifyingKey,
) -> Result<Collection<LwwRegisterBlob>>
where
    S: CollectionStoreExt + SnapshotSource,
    <S as SnapshotSource>::Snapshot: BlobStoreGet + CapabilityProofRead + CollectionRead,
{
    let source = crate::collection_names::open_configured(store, DEFAULT_SCOPE_ID, authority)?;
    intent_register_for_source(store, source)
}

pub(crate) fn intent_register_for_source<S>(
    store: &mut S,
    source: Collection<blobencodings::SimpleArchive>,
) -> Result<Collection<LwwRegisterBlob>>
where
    S: CollectionStoreExt + SnapshotSource,
    <S as SnapshotSource>::Snapshot: BlobStoreGet + CapabilityProofRead,
{
    let target =
        store.attach::<LwwRegisterBlob>(source, (metadata::tag.id(), metadata::created_at.id()))?;
    Ok(target)
}

fn fmt_id(id: Id) -> String {
    format!("{id:x}")
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

/// Build the canonical intrinsic record around already-stored capture
/// payload handles.
pub fn capture_record(row: &CaptureRow) -> Fragment {
    entity! {
        metadata::tag: &KIND_CAPTURE,
        metadata::created_at: row.created_at,
        capture::frame?: row.frame.as_ref(),
        capture::mime?: row.mime.as_ref(),
        capture::width?: row.width.as_ref(),
        capture::height?: row.height.as_ref(),
        capture::modality: row.modality,
        capture::note?: row.note.as_ref(),
        capture::pose: row.pose,
    }
}

/// Build the canonical intrinsic record around an already-stored intent text.
pub fn intent_record(row: &IntentRow) -> Fragment {
    entity! {
        metadata::tag: &KIND_INTENT,
        metadata::created_at: row.created_at,
        intent::text: row.text,
    }
}

/// The content handle of `value` as text, which is how a closed word is
/// compared against a stored one without reading the stored blob.
pub fn text_handle(value: &str) -> TextHandle {
    value.to_owned().to_blob().get_handle()
}

/// The capture modality word whose text has this handle, if it is one of the
/// three this faculty knows.
pub fn modality_name(handle: TextHandle) -> Option<&'static str> {
    ["vision", "audio", "touch"]
        .into_iter()
        .find(|name| text_handle(name) == handle)
}

fn validate_capture_shape(row: &CaptureRow) -> Result<()> {
    validate_point("capture creation time", row.created_at)?;
    match modality_name(row.modality) {
        Some("vision") => {
            if row.frame.is_none()
                || row.mime.is_none()
                || row.width.is_none()
                || row.height.is_none()
            {
                bail!(
                    "vision capture {} requires frame, MIME, width, and height",
                    fmt_id(row.id)
                );
            }
        }
        Some("audio") => {
            if row.frame.is_none() || row.mime.is_none() {
                bail!("audio capture {} requires frame and MIME", fmt_id(row.id));
            }
            if row.width.is_some() || row.height.is_some() {
                bail!(
                    "audio capture {} cannot carry image dimensions",
                    fmt_id(row.id)
                );
            }
        }
        Some("touch") => {
            if row.frame.is_some()
                || row.mime.is_some()
                || row.width.is_some()
                || row.height.is_some()
            {
                bail!(
                    "touch capture {} cannot carry frame, MIME, or image dimensions",
                    fmt_id(row.id)
                );
            }
        }
        _ => bail!("capture {} has unknown modality", fmt_id(row.id)),
    }
    Ok(())
}

/// Decode one capture open-world: a capture lacking a required field is not
/// one (`None`), and a field with several values yields the byte-smallest one
/// (the smallest tuple for the required fields).
pub fn decode_capture<P: TriblePattern>(space: &P, id: Id) -> Result<Option<CaptureRow>> {
    let Some((created_at, modality, pose)) = find!(
        (created_at: IntervalValue, modality: TextHandle, pose: TextHandle),
        pattern!(space, [{ id @
            metadata::created_at: ?created_at,
            capture::modality: ?modality,
            capture::pose: ?pose,
        }])
    )
    .min() else {
        return Ok(None);
    };
    let row = CaptureRow {
        id,
        created_at,
        frame: find!(
            value: RawHandle,
            pattern!(space, [{ id @ capture::frame: ?value }])
        )
        .min(),
        mime: find!(
            value: TextHandle,
            pattern!(space, [{ id @ capture::mime: ?value }])
        )
        .min(),
        width: find!(
            value: DimensionValue,
            pattern!(space, [{ id @ capture::width: ?value }])
        )
        .min(),
        height: find!(
            value: DimensionValue,
            pattern!(space, [{ id @ capture::height: ?value }])
        )
        .min(),
        modality,
        note: find!(
            value: TextHandle,
            pattern!(space, [{ id @ capture::note: ?value }])
        )
        .min(),
        pose,
    };
    validate_capture_shape(&row)?;
    Ok(Some(row))
}

/// Decode one intent with the same open-world reading as [`decode_capture`].
pub fn decode_intent<P: TriblePattern>(space: &P, id: Id) -> Result<Option<IntentRow>> {
    let Some((created_at, text)) = find!(
        (created_at: IntervalValue, text: TextHandle),
        pattern!(space, [{ id @
            metadata::created_at: ?created_at,
            intent::text: ?text,
        }])
    )
    .min() else {
        return Ok(None);
    };
    let row = IntentRow {
        id,
        created_at,
        text,
    };
    validate_point("intent creation time", row.created_at)?;
    Ok(Some(row))
}

/// Project the latest decodable intent through the exact maintained register.
///
/// The index answers which event wins the established total order; the typed
/// query then asks only for the fields this reader understands. If foreign
/// facts add several decodable spellings, their byte-smallest tuple is chosen
/// deterministically rather than turning unrelated history into a global
/// validation failure.
pub fn latest_intent<P: TriblePattern>(
    facts: &P,
    register: &LwwQuery,
) -> Result<Option<IntentRow>> {
    let Some(id) = register.winner(KIND_INTENT) else {
        return Ok(None);
    };
    Ok(find!(
        (created_at: IntervalValue, text: TextHandle),
        pattern!(facts, [{ id @
            metadata::tag: &KIND_INTENT,
            metadata::created_at: ?created_at,
            intent::text: ?text,
        }])
    )
    .map(|(created_at, text)| IntentRow {
        id,
        created_at,
        text,
    })
    .min_by_key(|row| (row.created_at, row.text)))
}

/// Attach Body's resident facts and known intent winners from one immutable
/// store observation. A read attaches what the maintenance worker has carried
/// and never maintains.
pub async fn materialize_indexed_collection(
    pile: &mut Pile,
    signer: &SigningKey,
) -> Result<BodySnapshot> {
    let source = open_configured(pile, DEFAULT_SCOPE_ID, signer.verifying_key())?;
    let succinct = pile.attach::<SuccinctArchiveBlob>(source, ())?;
    let rank9 = pile.attach::<Rank9AcceleratedSuccinctArchiveBlob>(source, succinct)?;
    let target = intent_register_collection(pile, signer.verifying_key())?;
    let store_snapshot = pile.snapshot().context("freeze resident Body targets")?;
    let facts = store_snapshot
        .read_facts(rank9)
        .context("read maintained Body fact collection")?;
    // The register over the same foundations the facts read, the commits
    // no attachment reaches built in memory.
    let intents = store_snapshot
        .attached(target)
        .map_err(|error| anyhow!("observe Body intent register: {error}"))?
        .read::<LwwIndex>()
        .map_err(|error| anyhow!("read Body intent register: {error}"))?
        .into_value()
        .query()
        .map_err(|error| anyhow!("prepare Body intent register query: {error}"))?;
    Ok(BodySnapshot {
        facts,
        store_snapshot,
        intents,
    })
}

/// The worker's carry for tests: the fact chain and the intent register.
#[cfg(test)]
pub(crate) fn carry_for_tests(pile: &mut Pile, signer: &SigningKey) {
    let source = open_configured(pile, DEFAULT_SCOPE_ID, signer.verifying_key()).unwrap();
    crate::storage::carry_facts(pile, source, signer);
    let target = intent_register_collection(pile, signer.verifying_key()).unwrap();
    drop(pollster::block_on(pile.maintain_attached(target, signer)).unwrap());
}

#[cfg(test)]
mod tests {
    use crate::storage::FactView;
    use hifitime::Epoch;
    use triblespace::core::id::ExclusiveId;
    use triblespace::core::inline::TryToInline;
    use triblespace::core::metadata;
    use triblespace::macros::entity;

    use super::*;

    fn at(seconds: f64) -> IntervalValue {
        let point = Epoch::from_tai_seconds(seconds);
        (point, point).try_to_inline().unwrap()
    }

    fn vision() -> Fragment {
        let mut fragment = Fragment::empty();
        let frame = fragment.put::<blobencodings::RawBytes, _>(vec![1, 2, 3]);
        let pose = fragment.put::<blobencodings::UTF8String, _>("{}".to_owned());
        let note = fragment.put::<blobencodings::UTF8String, _>("kept".to_owned());
        let mime = fragment.put::<blobencodings::UTF8String, _>("image/png".to_owned());
        let modality = fragment.put::<blobencodings::UTF8String, _>("vision".to_owned());
        fragment += capture_record(&CaptureRow {
            id: KIND_CAPTURE,
            created_at: at(1.0),
            frame: Some(frame),
            mime: Some(mime),
            width: Some(640_u64.to_inline()),
            height: Some(480_u64.to_inline()),
            modality,
            note: Some(note),
            pose,
        });
        fragment
    }

    #[test]
    fn resident_intent_views_allow_lag_without_changing_frozen_snapshots() {
        use triblespace::core::repo::{BlobStorePut, WantRead};

        pollster::block_on(async {
            std::env::remove_var(crate::collection_names::override_env_name(DEFAULT_SCOPE_ID));
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("body.pile");
            std::fs::File::create(&path).unwrap();
            let signer = SigningKey::from_bytes(&[32; 32]);
            let mut pile =
                crate::storage::open_pile_strict_as(&path, signer.verifying_key()).unwrap();
            let source =
                open_configured(&mut pile, DEFAULT_SCOPE_ID, signer.verifying_key()).unwrap();
            let succinct = pile.attach::<SuccinctArchiveBlob>(source, ()).unwrap();
            let rank9 = pile
                .attach::<Rank9AcceleratedSuccinctArchiveBlob>(source, succinct)
                .unwrap();
            let target = intent_register_collection(&mut pile, signer.verifying_key()).unwrap();
            let first = intent_record(&IntentRow {
                id: KIND_INTENT,
                created_at: at(1.0),
                text: pile
                    .put::<blobencodings::UTF8String, _>("first".to_owned())
                    .unwrap(),
            });
            let first_id = first.root().unwrap();
            pile.commit(source, &signer, first).unwrap();
            carry_for_tests(&mut pile, &signer);
            let before = materialize_indexed_collection(&mut pile, &signer)
                .await
                .unwrap();

            let second = intent_record(&IntentRow {
                id: KIND_INTENT,
                created_at: at(2.0),
                text: pile
                    .put::<blobencodings::UTF8String, _>("second".to_owned())
                    .unwrap(),
            });
            let second_id = second.root().unwrap();
            pile.commit(source, &signer, second).unwrap();
            drop(pile.maintain_attached(succinct, &signer).await.unwrap());
            let snapshot = pile.maintain_attached(rank9, &signer).await.unwrap();
            let fact_collection = snapshot.attached(rank9).unwrap();
            let intent_collection = snapshot.attached(target).unwrap();
            // The facts have attached the second intent; the register's
            // attachments lag the source by exactly that commit. Its cover
            // alone answers as it stands; the whole read builds the commit in
            // memory and answers like the facts.
            assert!(
                crate::storage::FactLag::of(&snapshot, source, succinct, rank9)
                    .unwrap()
                    .is_current()
            );
            assert_eq!(intent_collection.residual().len(), 1);
            let facts = fact_collection.facts().unwrap();
            let lagging = intent_collection
                .view::<LwwIndex>()
                .unwrap()
                .query()
                .unwrap();
            assert_eq!(
                latest_intent(&facts, &lagging).unwrap().unwrap().id,
                first_id
            );
            let whole = intent_collection.read::<LwwIndex>().unwrap();
            assert!(whole.unread().is_empty());
            assert_eq!(
                latest_intent(&facts, &whole.into_value().query().unwrap())
                    .unwrap()
                    .unwrap()
                    .id,
                second_id
            );

            let snapshot = pile.maintain_attached(target, &signer).await.unwrap();
            let advanced = snapshot
                .attached(target)
                .unwrap()
                .view::<LwwIndex>()
                .unwrap()
                .query()
                .unwrap();
            assert_eq!(latest_intent(before.facts(), &advanced).unwrap(), None);
            assert_eq!(
                latest_intent(&facts, &advanced).unwrap().unwrap().id,
                second_id
            );
            assert_eq!(
                latest_intent(&facts, &lagging).unwrap().unwrap().id,
                first_id
            );

            let admitted_before = source.admitted(&snapshot).unwrap();
            let after = materialize_indexed_collection(&mut pile, &signer)
                .await
                .unwrap();
            assert_eq!(
                latest_intent(after.facts(), after.intent_register())
                    .unwrap()
                    .unwrap()
                    .id,
                second_id,
            );
            assert_eq!(
                latest_intent(before.facts(), before.intent_register())
                    .unwrap()
                    .unwrap()
                    .id,
                first_id,
            );
            assert_eq!(
                source.admitted(after.store_snapshot()).unwrap(),
                admitted_before
            );
            assert!(after.store_snapshot().wants().unwrap().next().is_none());
            pile.close().unwrap();
        });
    }

    #[test]
    fn decoding_rejects_capture_shape_conflicts() {
        let pose: TextHandle = Inline::new([0xC3; 32]);
        let malformed = entity! {
            metadata::tag: &KIND_CAPTURE,
            metadata::created_at: at(1.0),
            capture::modality: "touch",
            capture::pose: pose,
            capture::mime: "image/png",
        };
        let id = malformed.root().unwrap();
        let error = decode_capture(malformed.facts(), id).unwrap_err();
        assert!(error.to_string().contains("touch capture"));
    }

    #[test]
    fn decoding_skips_a_capture_lacking_a_field_and_picks_among_repeated_values() {
        let canonical = vision();
        let id = canonical.root().unwrap();
        let bare = Id::new([0xD4; 16]).unwrap();
        let mut facts = canonical.facts().clone();
        facts += entity! { ExclusiveId::force_ref(&bare) @
            metadata::tag: &KIND_CAPTURE,
            metadata::created_at: at(2.0),
            capture::modality: "touch",
        };
        facts += entity! { ExclusiveId::force_ref(&id) @
            metadata::created_at: at(9.0),
            capture::note: "second",
        };
        assert!(decode_capture(&facts, bare).unwrap().is_none());
        let row = decode_capture(&facts, id).unwrap().unwrap();
        assert_eq!(row.created_at, at(1.0).min(at(9.0)));
        let notes: Vec<TextHandle> =
            find!(value: TextHandle, pattern!(&facts, [{ id @ capture::note: ?value }])).collect();
        assert_eq!(notes.len(), 2);
        assert_eq!(row.note, notes.into_iter().min());
    }
}
