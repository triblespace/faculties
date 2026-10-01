//! Portable exact-term-frequency BM25 over canonical Archive blocks.
//!
//! This module is one concrete attached-collection mapping, not a registry.
//! Its parent is Archive's canonical `SimpleArchive` union and its attached
//! representation is the portable BM25 carrier. Every canonical semantic block is a document,
//! including a genuine textless block. The unique content-free canonical
//! bottom used only by raw source receipts is excluded so provenance volume
//! cannot perturb corpus statistics. Content parts are occurrences, so the
//! same content fact at two ordinals contributes twice. Every selected
//! `UTF8String` payload is tokenized with [`hash_tokens`], and repeated
//! documents join by pointwise maximum in the portable carrier.
//!
//! Importer receipts are deliberately outside the projection. The mapping is
//! an open-world typed query: unknown facts and undecodable rows are inert.
//! A part a node's block references must be present with its content fact in
//! that node, or the node is refused; but a part the node does not reference
//! is invisible to it, so a node holding only some of a block's parts is
//! indexed from those parts, with nothing refused. See
//! [`ArchiveBlockTextBm25Mapping`] for what that means for a cover.

use std::collections::{BTreeMap, BTreeSet};

use anybytes::View;
use anyhow::{bail, Result};

use triblespace::core::blob::encodings::simplearchive::SimpleArchive;
use triblespace::core::blob::encodings::utf8string::UTF8String;
use triblespace::core::blob::{Blob, IntoBlob, TryFromBlob};
use triblespace::core::collection::records::{mapping_algorithm, KIND_COLLECTION_MAPPING};
use triblespace::core::collection::{CollectionData, CollectionOperationError, MapMapping};
use triblespace::core::id::{id_hex, Id};
use triblespace::core::inline::encodings::genid::GenId;
use triblespace::core::inline::encodings::hash::Handle;
use triblespace::core::inline::{Inline, IntoInline, RawInline};
use triblespace::core::metadata::{self, MetaDescribe};
use triblespace::core::repo::{BlobStoreGet, StoreRead};
use triblespace::core::trible::Fragment;
use triblespace::core::trible::TribleSet;
use triblespace::macros::entity;
use triblespace::prelude::blobencodings::RawBytes;
use triblespace::prelude::{find, pattern};
use triblespace_search::portable_bm25::{PortableBM25Blob, PortableBM25Index, PortableBM25View};
use triblespace_search::tokens::{hash_tokens, WordHash};

use crate::schemas::blockdag as schema;

/// Archive-block-text BM25 member mapping, version 1.
///
/// Minted with `trible genid` on 2026-08-30:
/// `4EC6991611EF484A37FBD95F6E108FC6`.
///
/// Changing the selected graph fields, occurrence aggregation / term-frequency
/// law, tokenizer behavior, or document/term schemas requires a new mapping id.
/// Joining mapped members belongs to [`PortableBM25Blob`], not this identity.
/// BM25 `k1` / `b` scoring policy is derived query behavior and is deliberately
/// outside the persisted collection identity.
pub const ARCHIVE_BLOCK_TEXT_BM25_MAPPING_V1: Id = id_hex!("4EC6991611EF484A37FBD95F6E108FC6");

pub type ArchiveBM25Index = PortableBM25Index<GenId, WordHash>;
/// Shared carrier views for one logical Archive search cover.
pub type ArchiveBM25View = PortableBM25View<GenId, WordHash>;

#[derive(Debug)]
enum DeriveValidation {
    Ready(Blob<PortableBM25Blob>),
    /// A selected text payload is unavailable; the first one, by handle.
    Pending(Inline<Handle<UTF8String>>),
    Rejected(String),
}

#[derive(Debug)]
struct ProjectionPlan {
    documents: BTreeMap<Id, Vec<Inline<Handle<UTF8String>>>>,
}

/// The archive-block-text BM25 law, as a describable type.
///
/// A descriptor embeds this rather than only naming it, so a reader holding
/// the pile can learn what the index is without the code that built it.
pub struct ArchiveBlockTextBm25MappingV1;

impl MetaDescribe for ArchiveBlockTextBm25MappingV1 {
    fn describe() -> triblespace::core::trible::Fragment {
        let id: Id = ARCHIVE_BLOCK_TEXT_BM25_MAPPING_V1;
        entity! {
            triblespace::core::id::ExclusiveId::force_ref(&id) @
                metadata::name: "archive-block-text-bm25-v1",
                metadata::description: "Canonical mapping from one Archive SimpleArchive member to one PortableBM25Blob. Every canonical semantic block in that member is one document; Archive's content-free bottom is excluded, selected UTF8String payload occurrences are tokenized with hash_tokens, repeated terms contribute exact frequencies, and repeated documents combine by pointwise maximum. PortableBM25Blob owns target-member validation and join. The k1 and b scoring parameters are deliberately absent because they are query-time behaviour.",
                metadata::tag: metadata::KIND_COLLECTION_MAPPING_ALGORITHM,
        }
    }
}

/// Bound canonical projection from one Archive fact-set member to its
/// portable BM25 image.
///
/// Its cover-query law ([`MapMapping`](triblespace::core::collection::MapMapping))
/// is a premise about its input, not something it checks. A block's term
/// frequency sums over the block's parts, and a node cannot tell that a
/// block has more parts elsewhere. So this mapping answers like the union
/// for every cover whose nodes hold whole source units (a block with all its
/// parts and their facts). A node holding part of a block scores that block
/// from the part it holds, with no refusal and no residual to show it, so a
/// cover that splits a block undercounts it even when nothing is unread.
/// Faculties' own Archive importer commits whole source units
/// ([`crate::archive_collection::ArchiveImportWriter::stage_fragment`] keeps
/// each fragment it is given whole in one commit, and the importers hand it
/// whole blocks), which `every_import_commit_holds_whole_blocks` pins. That
/// is the premise for data we write. It is not a guarantee: the writer does
/// not check that a fragment holds whole blocks, and another writer's
/// commits or historical input need not hold them.
///
/// The ignored tests `a_block_whose_parts_sit_in_two_nodes_scores_like_their_union`,
/// `a_block_extended_by_a_node_without_its_tag_scores_like_their_union` and
/// `archive_block_covers_answer_like_the_union_when_parts_spread_over_nodes`
/// are the acceptance tests of the structural fix: a per-fact text BM25 over
/// the content fact's payload, blocks ranked at query time through the fact
/// read, which retires this mapping (`4EC6991611EF484A37FBD95F6E108FC6`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArchiveBlockTextBm25Mapping;

impl MapMapping for ArchiveBlockTextBm25Mapping {
    type Target = PortableBM25Blob;

    fn fragment(&self) -> Fragment {
        mapping_fragment()
    }

    fn bind(_parent: &Fragment, attached: &Fragment) -> Result<Self, CollectionOperationError> {
        let actual = triblespace::core::collection::descriptor::mapping_algorithm(attached.facts())
            .map_err(|error| CollectionOperationError::Fatal(error.to_string()))?;
        if actual != Some(ARCHIVE_BLOCK_TEXT_BM25_MAPPING_V1) {
            return Err(CollectionOperationError::Fatal(format!(
                "Archive BM25 mapping algorithm {:?} does not match archive-block-text-v1 \
                 {ARCHIVE_BLOCK_TEXT_BM25_MAPPING_V1:X}",
                actual.map(|id| format!("{id:X}")),
            )));
        }
        Ok(Self)
    }

    /// Map one parent node, classifying what stops it by what could still
    /// change the answer. A selected text payload that is not here is a
    /// dependency the store may fetch, after which the map runs again. A
    /// node holding a block that references a part the node does not hold,
    /// or a part whose content fact is absent or untyped there, is refused
    /// as that node's capacity: maintenance descends to the nodes beneath
    /// it, and a merged node above that holds the whole closure represents
    /// it. Not every split is refused: a node holding a block with only some
    /// of its `contains` facts, or a `contains` fact without the block's
    /// tag, is mapped from what it holds (see the type's documentation).
    /// Only a failure to read the store is fatal, because that is the one
    /// thing no other node can get around.
    fn map<R: StoreRead>(
        &self,
        node: &Blob<SimpleArchive>,
        _siblings: &[CollectionData],
        reader: &R,
    ) -> Result<Blob<PortableBM25Blob>, CollectionOperationError> {
        match derive_for_validation(reader, node.clone()) {
            Ok(DeriveValidation::Ready(blob)) => Ok(blob),
            Ok(DeriveValidation::Pending(payload)) => Err(
                CollectionOperationError::MissingDependency(Inline::new(payload.raw)),
            ),
            Ok(DeriveValidation::Rejected(reason)) => Err(CollectionOperationError::Capacity(
                format!("invalid Archive BM25 source: {reason}"),
            )),
            Err(error) => Err(CollectionOperationError::Fatal(format!("{error:#}"))),
        }
    }
}

fn mapping_fragment() -> Fragment {
    entity! {
        metadata::tag: KIND_COLLECTION_MAPPING,
        mapping_algorithm*: <ArchiveBlockTextBm25MappingV1 as MetaDescribe>::describe(),
    }
}

/// Build one exact portable Archive BM25 element.
///
/// Selected text is read by its exact handle. A resident reader stays local;
/// an acquiring reader may obtain those bytes without advancing the selected
/// source node. Unavailable text is an operational cache miss for the builder.
pub fn derive_element<R>(reader: &R, source: Blob<SimpleArchive>) -> Result<Blob<PortableBM25Blob>>
where
    R: BlobStoreGet,
{
    match derive_for_validation(reader, source)? {
        DeriveValidation::Ready(blob) => Ok(blob),
        DeriveValidation::Pending(payload) => bail!(
            "Archive BM25 source has a nonresident text payload {}",
            hex::encode_upper(payload.raw)
        ),
        DeriveValidation::Rejected(reason) => bail!("invalid Archive BM25 source: {reason}"),
    }
}

fn derive_for_validation<R>(reader: &R, source: Blob<SimpleArchive>) -> Result<DeriveValidation>
where
    R: BlobStoreGet,
{
    let plan = match projection_plan(source) {
        Ok(plan) => plan,
        Err(reason) => return Ok(DeriveValidation::Rejected(reason)),
    };

    // Resolve each distinct payload once, while retaining its occurrence in
    // every part that names it. Try every required sibling before Pending so
    // malformed bytes cannot hide behind one unavailable payload. Do not test
    // frozen residency first: an acquiring reader can obtain exact text even
    // though this observation correctly keeps reporting it as nonresident.
    let handles: BTreeSet<_> = plan
        .documents
        .values()
        .flat_map(|payloads| payloads.iter().copied())
        .collect();
    let mut token_cache = BTreeMap::new();
    let mut missing = None;
    for handle in handles {
        let blob: Blob<UTF8String> = match reader.get(handle) {
            Ok(blob) => blob,
            Err(error) if triblespace::core::repo::is_missing_blob(&error) => {
                missing.get_or_insert(handle);
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        let text: View<str> = match blob.bytes.clone().view() {
            Ok(text) => text,
            Err(error) => {
                return Ok(DeriveValidation::Rejected(format!(
                    "resident UTF8String payload {} is not UTF-8: {error}",
                    hex::encode_upper(handle.raw),
                )))
            }
        };
        token_cache.insert(handle.raw, hash_tokens(text.as_ref()));
    }
    if let Some(payload) = missing {
        return Ok(DeriveValidation::Pending(payload));
    }

    let documents: Vec<Inline<GenId>> = plan.documents.keys().map(IntoInline::to_inline).collect();
    let mut counts = Vec::new();
    for (document_id, payloads) in plan.documents {
        let document: Inline<GenId> = document_id.to_inline();
        let mut frequencies: BTreeMap<RawInline, u32> = BTreeMap::new();
        for payload in payloads {
            let tokens = token_cache
                .get(&payload.raw)
                .expect("all selected payloads were resolved before counting");
            for token in tokens {
                let frequency = frequencies.entry(token.raw).or_default();
                let Some(incremented) = frequency.checked_add(1) else {
                    return Ok(DeriveValidation::Rejected(format!(
                        "term frequency overflows u32 for Archive block {document_id:X}"
                    )));
                };
                *frequency = incremented;
            }
        }
        counts.extend(
            frequencies
                .into_iter()
                .map(|(term, frequency)| (document, Inline::<WordHash>::new(term), frequency)),
        );
    }

    let index = match ArchiveBM25Index::from_exact_counts(documents, counts) {
        Ok(index) => index,
        Err(error) => {
            return Ok(DeriveValidation::Rejected(format!(
                "portable BM25 construction failed: {error}"
            )))
        }
    };
    Ok(DeriveValidation::Ready(index.to_blob()))
}

fn projection_plan(source: Blob<SimpleArchive>) -> std::result::Result<ProjectionPlan, String> {
    let facts = TribleSet::try_from_blob(source)
        .map_err(|error| format!("source is not a canonical SimpleArchive: {error}"))?;
    let block_ids: BTreeSet<Id> = find!(
        block: Id,
        pattern!(&facts, [{ ?block @ metadata::tag: &schema::block::KIND }])
    )
    .collect();
    let mut documents = BTreeMap::new();
    for block_id in block_ids {
        let part_ids: BTreeSet<Id> = find!(
            part: Id,
            pattern!(&facts, [{ block_id @ schema::block::contains: ?part }])
        )
        .collect();
        // The unique content-free bottom is not a BM25 document. Under the
        // open-world schema, other tag-only rows are simply nonmatching too;
        // neither case licenses reconstructing or validating an entity id.
        if part_ids.is_empty() {
            continue;
        }

        let mut parts = Vec::new();
        for part_id in part_ids {
            let occurrences: BTreeSet<(u64, Id)> = find!(
                (ordinal: u64, fact: Id),
                pattern!(&facts, [{
                    part_id @ metadata::tag: &schema::content_part::KIND,
                    schema::content_part::ordinal: ?ordinal,
                    schema::content_part::fact: ?fact,
                }])
            )
            .collect();
            if occurrences.is_empty() {
                return Err(format!(
                    "Archive block {block_id:X} references absent part {part_id:X} or one without typed fields"
                ));
            }
            for (ordinal, fact_id) in occurrences {
                if find!(
                    (modality: Id, direction: Id),
                    pattern!(&facts, [{
                        fact_id @ metadata::tag: &schema::content_fact::KIND,
                        schema::content_fact::modality: ?modality,
                        schema::content_fact::direction: ?direction,
                    }])
                )
                .next()
                .is_none()
                {
                    return Err(format!(
                        "Archive part {part_id:X} references absent or untyped fact {fact_id:X}"
                    ));
                }
                let payloads: Vec<Inline<Handle<UTF8String>>> = find!(
                    payload: Inline<Handle<UTF8String>>,
                    pattern!(&facts, [{ fact_id @ schema::content_fact::payload: ?payload }])
                )
                .collect();
                let has_nontext_payload = find!(
                    _blob: Inline<Handle<RawBytes>>,
                    pattern!(&facts, [{ fact_id @ schema::content_fact::blob: ?_blob }])
                )
                .next()
                .is_some()
                    || find!(
                        _pointer: Inline<Handle<UTF8String>>,
                        pattern!(&facts, [{
                            fact_id @ schema::content_fact::asset_pointer: ?_pointer
                        }])
                    )
                    .next()
                    .is_some();
                if payloads.is_empty() && !has_nontext_payload {
                    return Err(format!(
                        "Archive content fact {fact_id:X} has no typed payload variant"
                    ));
                }
                parts.push((ordinal, part_id, payloads));
            }
        }
        parts.sort_unstable_by_key(|(ordinal, part, _)| (*ordinal, *part));
        let payloads = parts
            .into_iter()
            .flat_map(|(_, _, payloads)| payloads)
            .collect();
        documents.insert(block_id, payloads);
    }
    Ok(ProjectionPlan { documents })
}

#[cfg(test)]
mod tests {
    use std::fs::File;

    use anybytes::Bytes;
    use tempfile::TempDir;
    use triblespace::core::blob::encodings::UnknownBlob;
    use triblespace::core::id::ExclusiveId;
    use triblespace::core::repo::pile::PileSnapshot;
    use triblespace::core::repo::{BlobStorePut, SnapshotSource};
    use triblespace::core::trible::Fragment;
    use triblespace::macros::entity;

    use super::*;
    use crate::blockdag as archive;

    struct StoredBlobs {
        _directory: TempDir,
        reader: PileSnapshot,
    }

    impl StoredBlobs {
        fn new(blobs: impl IntoIterator<Item = Blob<UnknownBlob>>) -> Self {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("archive-bm25-test.pile");
            File::create(&path).unwrap();
            let mut pile = crate::storage::open_pile_strict(&path).unwrap();
            for blob in blobs {
                pile.put::<UnknownBlob, _>(blob).unwrap();
            }
            pile.flush().unwrap();
            let reader = pile.snapshot().unwrap();
            pile.close().unwrap();
            Self {
                _directory: directory,
                reader,
            }
        }
    }

    fn source_and_attachments(fragment: Fragment) -> (Blob<SimpleArchive>, Vec<Blob<UnknownBlob>>) {
        let (facts, mut blobs) = fragment.into_facts_and_blobs();
        let source: Blob<SimpleArchive> = facts.to_blob();
        let attachments = blobs
            .snapshot()
            .unwrap()
            .into_iter()
            .map(|(_, blob)| blob)
            .collect();
        (source, attachments)
    }

    fn text_block(parts: &[(Id, &str)]) -> Fragment {
        let mut occurrences = Fragment::empty();
        for (ordinal, (modality, text)) in parts.iter().enumerate() {
            let fact = archive::text_fact(
                *modality,
                schema::content_fact::direction::IN,
                (*text).to_owned(),
            )
            .unwrap();
            occurrences += archive::content_part(ordinal as u64, fact, None).unwrap();
        }
        archive::block(std::iter::empty::<Id>(), None, occurrences).unwrap()
    }

    fn parse(blob: Blob<PortableBM25Blob>) -> ArchiveBM25Index {
        ArchiveBM25Index::try_from_blob(blob).unwrap()
    }

    fn derive(reader: &PileSnapshot, source: Blob<SimpleArchive>) -> Blob<PortableBM25Blob> {
        derive_element(reader, source).unwrap()
    }

    #[test]
    fn a_cold_residual_block_acquires_its_text_without_changing_residency() {
        use triblespace::core::collection::{
            AdmissionPolicy, CollectionPolicy, CollectionSnapshotExt,
            CollectionStoreExt,
        };
        use triblespace::core::repo::{BlobStoreList, StorageClose};

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cold-archive-bm25.pile");
        File::create(&path).unwrap();
        let signer = ed25519_dalek::SigningKey::from_bytes(&[113; 32]);
        let mut store = crate::storage::open_store_as(&path, signer.verifying_key()).unwrap();
        let root = store.collection("cold Archive BM25", CollectionPolicy::new(
            AdmissionPolicy::direct(signer.verifying_key()),
            AdmissionPolicy::direct(signer.verifying_key()),
        )).unwrap();
        let target = store.attach_with(root, ArchiveBlockTextBm25Mapping).unwrap();
        let text: Blob<UTF8String> = "late comet comet".to_blob();
        let text_handle = text.get_handle();
        let (node, attachments) = source_and_attachments(text_block(&[
            (schema::content_fact::modality::TEXT, "late comet comet"),
        ]));
        assert!(attachments.iter().any(|blob| blob.get_handle().raw == text_handle.raw));
        let facts = TribleSet::try_from_blob(node.clone()).unwrap();
        store.commit(root, &signer, facts.into()).unwrap();
        let frozen = store.snapshot().unwrap();
        assert!(matches!(
            derive_for_validation(&frozen, node.clone()).unwrap(),
            DeriveValidation::Pending(handle) if handle == text_handle
        ));
        // Descriptor registration may already have stored schema attachments.
        // The selected document text itself must be genuinely cold.
        assert!(!frozen.contains_blob(text_handle).unwrap());

        // Exact lookup can now find the bytes in the owning Leech, but the
        // selected node and residency observation remain the older ones.
        for blob in attachments.iter().cloned() {
            store.put::<UnknownBlob, _>(blob).unwrap();
        }
        let runtime = std::sync::Arc::new(crate::storage::runtime().unwrap());
        let reader = crate::storage::AcquiringReader::new(frozen, runtime);
        let selected = reader.attached_acquiring(target).unwrap();
        assert_eq!(selected.residual().len(), 1);
        let index = crate::storage::require_complete_attached_read(
            selected.read_with_acquiring::<ArchiveBlockTextBm25Mapping, ArchiveBM25View>().unwrap(),
        ).unwrap();
        assert_eq!(index.segments().iter().map(|segment| segment.doc_count()).sum::<usize>(), 1);
        assert!(!reader.contains_blob(text_handle).unwrap());
        assert!(store.health().started_at.is_none());
        store.close().unwrap();
    }

    #[test]
    fn mapping_v1_freezes_case_punctuation_and_unicode_tokenization() {
        let actual: Vec<_> = hash_tokens("Hello, WORLD — hello. 🛰️")
            .into_iter()
            .map(|token| hex::encode_upper(token.raw))
            .collect();
        assert_eq!(
            actual,
            [
                "EA8F163DB38682925E4491C5E58D4BB3506EF8C14EB78A86E908C5624A67200F",
                "D7894AE9716D38D2DFAD0EC55424CA321EE12453D51F1B3ADEB77D0475ED988C",
                "EA8F163DB38682925E4491C5E58D4BB3506EF8C14EB78A86E908C5624A67200F",
                "A7908C180BA54AD5F231D25EA710B3C5C4485B8445F0E9D95ECA460DCA3A966E",
            ]
        );
        assert_eq!(
            hex::encode_upper(hash_tokens("CAFÉ…")[0].raw),
            "9B79CF554A46C059EE5892ECB71EDAC015A8461816ABF33A5F7936087F5669E6"
        );
    }

    #[test]
    fn empty_corpus_and_semantic_textless_blocks_remain_documents() {
        let (empty_source, empty_attachments) = source_and_attachments(Fragment::empty());
        let empty_store = StoredBlobs::new(empty_attachments);
        let empty = parse(derive(&empty_store.reader, empty_source));
        assert_eq!(empty.doc_count(), 0);
        assert_eq!(empty.term_count(), 0);

        let empty_text = text_block(&[(schema::content_fact::modality::TEXT, "")]);
        let empty_text_id = empty_text.root().unwrap();
        let binary_fact = archive::blob_fact(
            schema::content_fact::modality::IMAGE,
            schema::content_fact::direction::IN,
            vec![0, 1, 2, 3],
            "application/octet-stream",
        )
        .unwrap();
        let binary_part = archive::content_part(0, binary_fact, None).unwrap();
        let binary_block = archive::block(std::iter::empty::<Id>(), None, binary_part).unwrap();
        let binary_id = binary_block.root().unwrap();
        let bottom = archive::block(std::iter::empty::<Id>(), None, Fragment::empty()).unwrap();
        let mut corpus = empty_text;
        corpus += binary_block;
        corpus += bottom;
        let (source, attachments) = source_and_attachments(corpus);
        let store = StoredBlobs::new(attachments);
        let index = parse(derive(&store.reader, source));
        let documents: BTreeSet<_> = index.document_keys().map(|doc| doc.raw).collect();
        assert_eq!(index.doc_count(), 2);
        assert_eq!(index.term_count(), 0);
        let empty_text_inline: Inline<GenId> = empty_text_id.to_inline();
        let binary_inline: Inline<GenId> = binary_id.to_inline();
        assert_eq!(
            documents,
            BTreeSet::from([empty_text_inline.raw, binary_inline.raw])
        );
    }

    #[test]
    fn canonical_bottom_does_not_perturb_the_bm25_carrier() {
        let semantic = text_block(&[(schema::content_fact::modality::TEXT, "stable corpus")]);
        let (semantic_source, semantic_attachments) = source_and_attachments(semantic.clone());
        let semantic_store = StoredBlobs::new(semantic_attachments);
        let semantic_index = parse(derive(&semantic_store.reader, semantic_source));

        let mut with_bottom = semantic;
        with_bottom += archive::block(std::iter::empty::<Id>(), None, Fragment::empty()).unwrap();
        let (with_bottom_source, with_bottom_attachments) = source_and_attachments(with_bottom);
        let with_bottom_store = StoredBlobs::new(with_bottom_attachments);
        let with_bottom_index = parse(derive(&with_bottom_store.reader, with_bottom_source));

        assert_eq!(semantic_index, with_bottom_index);
    }

    #[test]
    fn content_free_nonmatching_rows_are_inert() {
        let predecessor = text_block(&[(schema::content_fact::modality::TEXT, "parent")]);
        let predecessor_id = predecessor.root().unwrap();
        let mut invalid = entity! { _ @
            schema::block::previous: &predecessor_id,
        };
        let invalid_id = invalid.root().unwrap();
        invalid += entity! { ExclusiveId::force_ref(&invalid_id) @
            metadata::tag: &schema::block::KIND,
        };

        let mut corpus = predecessor;
        corpus += invalid;
        let (source, attachments) = source_and_attachments(corpus);
        let store = StoredBlobs::new(attachments);
        let index = parse(derive(&store.reader, source));
        assert_eq!(index.doc_count(), 1);
        let predecessor_inline: Inline<GenId> = predecessor_id.to_inline();
        assert_eq!(
            index
                .document_keys()
                .map(|document| document.raw)
                .collect::<Vec<_>>(),
            vec![predecessor_inline.raw],
        );
    }

    #[test]
    fn repeated_part_occurrences_sum_across_modalities() {
        let block = text_block(&[
            (schema::content_fact::modality::THINKING, "echo"),
            (schema::content_fact::modality::TOOL_RESULT, "echo"),
        ]);
        let block_id = block.root().unwrap();
        let (source, attachments) = source_and_attachments(block);
        let store = StoredBlobs::new(attachments);
        let index = parse(derive(&store.reader, source));
        let document: Inline<GenId> = block_id.to_inline();
        let term = hash_tokens("echo")[0];
        assert_eq!(index.term_frequency(&document, &term).unwrap(), 2);
        assert_eq!(index.merged(&index).unwrap(), index);
    }

    #[test]
    fn derivation_commutes_with_union_and_carrier_join() {
        let left = text_block(&[(schema::content_fact::modality::TEXT, "alpha alpha")]);
        let right = text_block(&[(schema::content_fact::modality::TEXT, "beta")]);
        let left_source: Blob<SimpleArchive> = left.facts().clone().to_blob();
        let right_source: Blob<SimpleArchive> = right.facts().clone().to_blob();
        let mut union = left;
        union += right;
        let (union_source, attachments) = source_and_attachments(union);
        let store = StoredBlobs::new(attachments);

        let left = parse(derive(&store.reader, left_source));
        let right = parse(derive(&store.reader, right_source));
        let direct = derive(&store.reader, union_source);
        let merged: Blob<PortableBM25Blob> = left.merged(&right).unwrap().to_blob();
        let reverse: Blob<PortableBM25Blob> = right.merged(&left).unwrap().to_blob();
        assert_eq!(merged.bytes, direct.bytes);
        assert_eq!(reverse.bytes, direct.bytes);
    }

    #[test]
    fn selected_missing_payload_and_malformed_encoding_fail_derivation() {
        let block = text_block(&[(schema::content_fact::modality::TEXT, "not resident")]);
        let (source, _attachments) = source_and_attachments(block);
        let missing_store = StoredBlobs::new([]);
        let error = derive_element(&missing_store.reader, source).unwrap_err();
        assert!(format!("{error:#}").contains("nonresident text payload"));

        let fact = archive::text_fact(
            schema::content_fact::modality::TEXT,
            schema::content_fact::direction::IN,
            "open world",
        )
        .unwrap();
        let part = archive::content_part(0, fact, None).unwrap();
        let part_id = part.root().unwrap();
        let block_id = Id::new([0xA5; 16]).unwrap();
        let mut graph_with_unknown_fact = part;
        graph_with_unknown_fact += entity! { ExclusiveId::force_ref(&block_id) @
            metadata::tag: &schema::block::KIND,
            metadata::name: "unexpected block field",
            schema::block::contains: &part_id,
        };
        let (graph_with_unknown_fact, attachments) =
            source_and_attachments(graph_with_unknown_fact);
        let open_world_store = StoredBlobs::new(attachments);
        let index = parse(derive(&open_world_store.reader, graph_with_unknown_fact));
        let block_inline: Inline<GenId> = block_id.to_inline();
        assert_eq!(
            index.doc_count(),
            1,
            "unknown facts do not close the schema"
        );
        assert_eq!(
            index
                .document_keys()
                .map(|document| document.raw)
                .collect::<Vec<_>>(),
            vec![block_inline.raw],
            "the selected entity id remains opaque",
        );

        let malformed = Blob::<SimpleArchive>::new(Bytes::from(vec![0xFF]));
        let error = derive_element(&open_world_store.reader, malformed).unwrap_err();
        assert!(format!("{error:#}").contains("canonical SimpleArchive"));
    }

    /// A seeded splitmix64 stream, for reproducible random lattices.
    struct Stream(u64);

    impl Stream {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        fn below(&mut self, bound: u64) -> u64 {
            self.next() % bound
        }
    }

    /// One block cut in two: its own facts, and its parts and facts with
    /// every text payload.
    fn split(block: Fragment) -> (Fragment, Fragment) {
        let block_id = block.root().unwrap();
        let (_, facts, metafacts, blobs) = block.into_parts();
        let mut own = TribleSet::new();
        let mut rest = TribleSet::new();
        for fact in facts.iter() {
            if fact.e() == &block_id {
                own.insert(fact);
            } else {
                rest.insert(fact);
            }
        }
        (
            Fragment::from(own),
            Fragment::from_parts(rest, metafacts, blobs),
        )
    }

    /// Scores per document for one query, rounded to compare across two
    /// summation orders.
    fn scores(view: &ArchiveBM25View, query: &str) -> BTreeMap<RawInline, i64> {
        view.query()
            .unwrap()
            .query_multi(&hash_tokens(query))
            .into_iter()
            .map(|(document, score)| (document.raw, (f64::from(score) * 1e4).round() as i64))
            .collect()
    }

    /// The cover-query equivalence law for the archive-block mapping over
    /// the one split it refuses: a block's own facts in one commit and its
    /// parts and facts in another. The commit holding the block references
    /// parts it lacks and cannot be indexed alone (`Capacity`), only from a
    /// node holding both. Splits the mapping does not refuse are the ignored
    /// tests below.
    ///
    /// Over random commits (some blocks split), the host's carry or not,
    /// random host merges over possibly overlapping nodes, and an attachment
    /// at random nodes wherever the mapping can build one, the production
    /// read -- the attached cover and the residual built in memory -- scores
    /// every query exactly as the index of the union of every foundation it
    /// reads. What it cannot read it names, and only a node that cannot be
    /// indexed alone is ever unread.
    #[test]
    fn archive_block_covers_answer_like_the_index_of_what_they_read() {
        use ed25519_dalek::SigningKey;
        use triblespace::core::collection::{
            simplearchive_union, AdmissionPolicy, CollectionMap, CollectionMerge, CollectionPolicy,
            CollectionRecord, CollectionSnapshotExt, CollectionStore, CollectionStoreExt,
            CoverageRead, TryFromCover,
        };
        use triblespace::core::repo::memoryrepo::MemoryRepo;

        const WORDS: [&str; 5] = ["alpha", "bravo", "charlie", "delta", "echo"];
        let key = SigningKey::from_bytes(&[61; 32]);
        let host = key.verifying_key();
        let mut unread_seen = 0;
        let mut attachments_taken = 0;
        for seed in 0..24u64 {
            let mut random = Stream(seed.wrapping_mul(0x2545_F491_4F6C_DD1D) + 7);
            let mut store = MemoryRepo::for_host(host);
            let policy =
                CollectionPolicy::new(AdmissionPolicy::direct(host), AdmissionPolicy::direct(host));
            let root = store
                .collection(&format!("archive-law-{seed}"), policy)
                .unwrap();
            let target = store
                .attach_with(root, ArchiveBlockTextBm25Mapping)
                .unwrap();
            for _ in 0..2 + random.below(8) {
                let words: Vec<&str> = (0..1 + random.below(3))
                    .map(|_| WORDS[random.below(WORDS.len() as u64) as usize])
                    .collect();
                let block = text_block(&[(schema::content_fact::modality::TEXT, &words.join(" "))]);
                let pieces = if random.below(3) == 0 {
                    let (own, rest) = split(block);
                    vec![own, rest]
                } else {
                    vec![block]
                };
                for piece in pieces {
                    let (_, attachments) = source_and_attachments(piece.clone());
                    for blob in attachments {
                        store.put::<UnknownBlob, _>(blob).unwrap();
                    }
                    store.commit(root, &key, piece).unwrap();
                }
            }
            if random.below(2) == 0 {
                pollster::block_on(store.maintain(root, &key)).unwrap();
            }
            let nodes = |store: &mut MemoryRepo| {
                let snapshot = store.snapshot().unwrap();
                let coverage =
                    CoverageRead::coverage(&snapshot, &BTreeSet::from([root.handle()])).unwrap();
                let mut seen = BTreeSet::new();
                let mut pending: Vec<CollectionData> = coverage.frontier(root.handle()).collect();
                while let Some(node) = pending.pop() {
                    if seen.insert(node) {
                        for inputs in coverage.producers(root.handle(), node) {
                            pending.extend(inputs.iter());
                        }
                    }
                }
                seen.into_iter().collect::<Vec<_>>()
            };
            // Host merges over random, possibly overlapping, nodes: some hold
            // both halves of a split block, some only one.
            for _ in 0..random.below(5) {
                let lattice = nodes(&mut store);
                if lattice.len() < 2 {
                    break;
                }
                let picked: BTreeSet<CollectionData> = (0..2 + random.below(3))
                    .map(|_| lattice[random.below(lattice.len() as u64) as usize])
                    .collect();
                if picked.len() < 2 {
                    continue;
                }
                let snapshot = store.snapshot().unwrap();
                let blobs: Vec<Blob<SimpleArchive>> = picked
                    .iter()
                    .map(|node| {
                        snapshot
                            .get(Handle::<SimpleArchive>::from_hash(*node))
                            .unwrap()
                    })
                    .collect();
                drop(snapshot);
                let joined = blobs.iter().skip(1).fold(blobs[0].clone(), |joined, blob| {
                    simplearchive_union::join(&joined, blob).unwrap()
                });
                let result = store.put::<SimpleArchive, _>(joined).unwrap();
                store
                    .insert(CollectionRecord::Merge(
                        CollectionMerge::sign(
                            &key,
                            root.handle(),
                            picked.iter().copied(),
                            Handle::<SimpleArchive>::to_hash(result),
                        )
                        .unwrap(),
                    ))
                    .unwrap();
            }
            // An attachment at random nodes, wherever one can be built.
            for node in nodes(&mut store) {
                if random.below(2) == 0 {
                    continue;
                }
                let snapshot = store.snapshot().unwrap();
                let bytes: Blob<SimpleArchive> = snapshot
                    .get(Handle::<SimpleArchive>::from_hash(node))
                    .unwrap();
                let image = ArchiveBlockTextBm25Mapping.map(&bytes, &[], &snapshot);
                drop(snapshot);
                let Ok(image) = image else {
                    continue;
                };
                let attachment = store.put::<PortableBM25Blob, _>(image).unwrap();
                store
                    .insert(CollectionRecord::Map(CollectionMap::sign(
                        &key,
                        target.handle(),
                        node,
                        Handle::<PortableBM25Blob>::to_hash(attachment),
                    )))
                    .unwrap();
            }

            let snapshot = store.snapshot().unwrap();
            let attached = snapshot.attached(target).unwrap();
            let read = attached
                .read_with::<ArchiveBlockTextBm25Mapping, ArchiveBM25View>()
                .unwrap();
            // What is unread is only ever a node no mapping can index alone.
            for member in read.unread().members() {
                let bytes: Blob<SimpleArchive> = snapshot.get(member).unwrap();
                assert!(
                    matches!(
                        ArchiveBlockTextBm25Mapping.map(&bytes, &[], &snapshot),
                        Err(CollectionOperationError::Capacity(_))
                    ),
                    "seed {seed}"
                );
                unread_seen += 1;
            }
            // The index of the union of every foundation the read includes.
            let mut union = TribleSet::new();
            for member in attached
                .support()
                .members()
                .chain(attached.residual().members())
            {
                if read.unread().contains(member) {
                    continue;
                }
                let bytes: Blob<SimpleArchive> = snapshot.get(member).unwrap();
                union += TribleSet::try_from_blob(bytes).unwrap();
            }
            let expected = ArchiveBlockTextBm25Mapping
                .map(&union.to_blob(), &[], &snapshot)
                .unwrap();
            attachments_taken += attached.cover().len();
            drop(attached);
            let expected = store.put::<PortableBM25Blob, _>(expected).unwrap();
            let snapshot = store.snapshot().unwrap();
            let descriptor: Blob<SimpleArchive> = snapshot.get(target.handle()).unwrap();
            let descriptor = Fragment::from(TribleSet::try_from_blob(descriptor).unwrap());
            let through_union: ArchiveBM25View =
                TryFromCover::try_from_cover(&target.cover([expected]), &descriptor, &snapshot)
                    .unwrap();
            assert_eq!(
                read.value().query().unwrap().doc_count(),
                through_union.query().unwrap().doc_count(),
                "seed {seed}"
            );
            for word in WORDS {
                assert_eq!(
                    scores(read.value(), word),
                    scores(&through_union, word),
                    "seed {seed}, query {word}"
                );
            }
            assert_eq!(
                scores(read.value(), "alpha charlie echo"),
                scores(&through_union, "alpha charlie echo"),
                "seed {seed}"
            );
        }
        // The seeds exercise both sides of the law: split blocks left unread,
        // and attachments taken.
        assert!(unread_seen > 0);
        assert!(attachments_taken > 0);
    }

    // The three ignored tests below show the cover law failing when a
    // block's parts span nodes. They are the acceptance tests of the
    // structural fix: a per-fact text BM25 over the content fact's payload,
    // with blocks ranked at query time through the fact read, which retires
    // mapping 4EC6991611EF484A37FBD95F6E108FC6. They stop being ignored with
    // it; until then this mapping's law holds only for covers of whole source
    // units (see `ArchiveBlockTextBm25Mapping`).

    /// One block of several text parts, and each part's fragment.
    fn parts_block(texts: &[&str]) -> (Fragment, Vec<Fragment>) {
        let mut parts = Vec::new();
        let mut occurrences = Fragment::empty();
        for (ordinal, text) in texts.iter().enumerate() {
            let fact = archive::text_fact(
                schema::content_fact::modality::TEXT,
                schema::content_fact::direction::IN,
                (*text).to_owned(),
            )
            .unwrap();
            let part = archive::content_part(ordinal as u64, fact, None).unwrap();
            parts.push(part.clone());
            occurrences += part;
        }
        let block = archive::block(std::iter::empty::<Id>(), None, occurrences).unwrap();
        (block, parts)
    }

    /// The facts of `block` a node holds when it has the `contains` fact and
    /// the whole closure of each of `parts`, and, when `tagged`, every other
    /// fact of the block itself.
    fn block_piece(block: &Fragment, parts: &[&Fragment], tagged: bool) -> Fragment {
        let block_id = block.root().unwrap();
        let contains = schema::block::contains.id();
        let mut facts = TribleSet::new();
        for part in parts {
            facts += part.facts().clone();
            let part_id: Inline<GenId> = part.root().unwrap().to_inline();
            for fact in block.facts().iter() {
                if fact.e() == &block_id
                    && fact.a() == &contains
                    && fact.v::<GenId>().raw == part_id.raw
                {
                    facts.insert(fact);
                }
            }
        }
        if tagged {
            for fact in block.facts().iter() {
                if fact.e() == &block_id && fact.a() != &contains {
                    facts.insert(fact);
                }
            }
        }
        Fragment::from(facts)
    }

    /// Commit each node, attach every one of them, and score each query
    /// through the production read and through the index of the union of
    /// the nodes. Every node is attached and nothing is unread, so the two
    /// must agree.
    fn scores_read_and_of_the_union(
        nodes: &[Fragment],
        texts: Vec<Blob<UnknownBlob>>,
        queries: &[&str],
    ) -> Vec<(BTreeMap<RawInline, i64>, BTreeMap<RawInline, i64>)> {
        use ed25519_dalek::SigningKey;
        use triblespace::core::collection::{
            AdmissionPolicy, CollectionPolicy, CollectionSnapshotExt, CollectionStoreExt,
            TryFromCover,
        };
        use triblespace::core::repo::memoryrepo::MemoryRepo;

        let key = SigningKey::from_bytes(&[62; 32]);
        let host = key.verifying_key();
        let mut store = MemoryRepo::for_host(host);
        let policy =
            CollectionPolicy::new(AdmissionPolicy::direct(host), AdmissionPolicy::direct(host));
        let root = store.collection("archive-parts", policy).unwrap();
        let target = store
            .attach_with(root, ArchiveBlockTextBm25Mapping)
            .unwrap();
        for blob in texts {
            store.put::<UnknownBlob, _>(blob).unwrap();
        }
        let mut union = TribleSet::new();
        for node in nodes {
            union += node.facts().clone();
            store.commit(root, &key, node.clone()).unwrap();
        }
        pollster::block_on(store.ensure_attached_with::<ArchiveBlockTextBm25Mapping>(target, &key))
            .unwrap();

        let snapshot = store.snapshot().unwrap();
        let attached = snapshot.attached(target).unwrap();
        assert!(attached.residual().is_empty(), "every node is attached");
        let read = attached
            .read_with::<ArchiveBlockTextBm25Mapping, ArchiveBM25View>()
            .unwrap();
        assert!(read.unread().is_empty());
        let expected = ArchiveBlockTextBm25Mapping
            .map(&union.to_blob(), &[], &snapshot)
            .unwrap();
        drop(attached);
        let expected = store.put::<PortableBM25Blob, _>(expected).unwrap();
        let snapshot = store.snapshot().unwrap();
        let descriptor: Blob<SimpleArchive> = snapshot.get(target.handle()).unwrap();
        let descriptor = Fragment::from(TribleSet::try_from_blob(descriptor).unwrap());
        let through_union: ArchiveBM25View =
            TryFromCover::try_from_cover(&target.cover([expected]), &descriptor, &snapshot)
                .unwrap();
        queries
            .iter()
            .map(|query| (scores(read.value(), query), scores(&through_union, query)))
            .collect()
    }

    /// Two nodes each hold the block and one of its two parts, each part
    /// with the same word. Each node maps, to a frequency of one; the union
    /// holds both parts, a frequency of two. The cover's pointwise maximum
    /// says one; a sum would say two here, and twice the truth wherever the
    /// two nodes hold the same part.
    #[test]
    #[ignore = "open design question: a block's term frequency sums over its parts, and a node \
                holding some of a block's parts cannot tell; no combination of per-node images \
                is exact"]
    fn a_block_whose_parts_sit_in_two_nodes_scores_like_their_union() {
        let (block, parts) = parts_block(&["echo", "echo"]);
        let (other, _) = parts_block(&["alpha"]);
        let (_, mut texts) = source_and_attachments(block.clone());
        texts.extend(source_and_attachments(other.clone()).1);
        let mut first = block_piece(&block, &[&parts[0]], true);
        first += other;
        let second = block_piece(&block, &[&parts[1]], true);
        for (read, union) in scores_read_and_of_the_union(&[first, second], texts, &["echo"]) {
            assert_eq!(read, union);
        }
    }

    /// The same block, one node holding it with its first part and another
    /// holding only its `contains` fact for the second part and that part's
    /// closure, not the block's tag. The second node has no document, so
    /// the block is in one image only, and still its union's frequency is
    /// larger: finding the blocks two images share cannot find every block
    /// the cover gets wrong.
    #[test]
    #[ignore = "open design question: a block's term frequency sums over its parts, and a node \
                holding some of a block's parts cannot tell; no combination of per-node images \
                is exact"]
    fn a_block_extended_by_a_node_without_its_tag_scores_like_their_union() {
        let (block, parts) = parts_block(&["echo", "echo"]);
        let (other, _) = parts_block(&["alpha"]);
        let (_, mut texts) = source_and_attachments(block.clone());
        texts.extend(source_and_attachments(other.clone()).1);
        let mut first = block_piece(&block, &[&parts[0]], true);
        first += other;
        let second = block_piece(&block, &[&parts[1]], false);
        for (read, union) in scores_read_and_of_the_union(&[first, second], texts, &["echo"]) {
            assert_eq!(read, union);
        }
    }

    /// The cover-query equivalence law over blocks of several parts whose
    /// facts are spread over nodes: whole, one part per node with the
    /// block's own facts in each, two overlapping runs of parts, or the
    /// block's own facts with its first part and each other part's
    /// `contains` fact and closure elsewhere; the pieces of different
    /// blocks share nodes at random. Every node maps, so the law must hold
    /// with nothing unread.
    #[test]
    #[ignore = "open design question: a block's term frequency sums over its parts, and a node \
                holding some of a block's parts cannot tell; no combination of per-node images \
                is exact"]
    fn archive_block_covers_answer_like_the_union_when_parts_spread_over_nodes() {
        const WORDS: [&str; 4] = ["alpha", "bravo", "charlie", "echo"];
        for seed in 0..24u64 {
            let mut random = Stream(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) + 5);
            let mut pieces = Vec::new();
            let mut texts = Vec::new();
            for _ in 0..1 + random.below(4) {
                let words: Vec<String> = (0..1 + random.below(3))
                    .map(|_| {
                        (0..1 + random.below(2))
                            .map(|_| WORDS[random.below(WORDS.len() as u64) as usize])
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .collect();
                let words: Vec<&str> = words.iter().map(String::as_str).collect();
                let (block, parts) = parts_block(&words);
                texts.extend(source_and_attachments(block.clone()).1);
                let all: Vec<&Fragment> = parts.iter().collect();
                match random.below(4) {
                    0 => pieces.push(block),
                    1 => {
                        for part in &all {
                            pieces.push(block_piece(&block, &[*part], true));
                        }
                    }
                    2 => {
                        let cut = random.below(all.len() as u64) as usize;
                        pieces.push(block_piece(&block, &all[..=cut], true));
                        pieces.push(block_piece(&block, &all[cut..], true));
                    }
                    _ => {
                        pieces.push(block_piece(&block, &all[..1], true));
                        for part in &all[1..] {
                            pieces.push(block_piece(&block, &[*part], false));
                        }
                    }
                }
            }
            let width = 1 + random.below(pieces.len() as u64) as usize;
            let mut nodes = vec![Fragment::empty(); width];
            for piece in pieces {
                let at = random.below(width as u64) as usize;
                nodes[at] += piece;
            }
            nodes.retain(|node| !node.facts().is_empty());
            for ((read, union), word) in scores_read_and_of_the_union(&nodes, texts, &WORDS)
                .into_iter()
                .zip(WORDS)
            {
                assert_eq!(read, union, "seed {seed}, query {word}");
            }
        }
    }
}
