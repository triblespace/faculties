//! Portable exact-term-frequency BM25 over the texts the Archive holds.
//!
//! This module is one concrete attached-collection mapping, not a registry.
//! Its parent is Archive's canonical `SimpleArchive` union and its attached
//! representation is the portable BM25 carrier. A document is one text
//! payload blob, keyed by its `Handle<UTF8String>`, exactly as the semantic
//! HNSW index keys its documents on embedding handles. A member's image is
//! every distinct value of [`schema::content_fact::payload`] it names, each
//! tokenized once with [`hash_tokens`] and counted with exact frequencies; an
//! empty text is still a document.
//!
//! A text blob is whole by definition: its hash fixes its terms. So the same
//! text named by several members, nodes or facts is one document with one
//! set of frequencies, segments join as a plain union (the carrier's
//! pointwise maximum never meets two different counts for one document), and
//! every cover answers like the index of the union of what it reads, however
//! a writer spreads a node's facts over commits. Readers rank texts and join
//! each one to the Archive nodes holding it at query time, then widen a node
//! to its act with [`crate::blockdag::turn_of`].
//!
//! The mapping reads the member's rows in place and never decodes them into
//! a `TribleSet`: it needs only the payload handles, and a merge node can hold
//! gigabytes of tribles. It is an open-world typed query: rows under other
//! attributes, nodes and facts in any shape, and importer receipts are inert.

use std::collections::{BTreeMap, BTreeSet};

use anybytes::View;
use anyhow::{bail, Result};

use triblespace::core::blob::encodings::simplearchive::SimpleArchive;
use triblespace::core::blob::encodings::utf8string::UTF8String;
use triblespace::core::blob::{Blob, IntoBlob};
use triblespace::core::collection::records::{mapping_algorithm, KIND_COLLECTION_MAPPING};
use triblespace::core::collection::{
    simplearchive_union, CollectionData, CollectionOperationError, MapMapping,
};
use triblespace::core::id::{id_hex, Id};
use triblespace::core::inline::encodings::hash::Handle;
use triblespace::core::inline::{Inline, RawInline};
use triblespace::core::metadata::{self, MetaDescribe};
use triblespace::core::repo::{BlobStoreGet, StoreRead};
use triblespace::core::trible::{Fragment, A_START, TRIBLE_LEN, V_START};
use triblespace::macros::entity;
use triblespace_search::portable_bm25::{PortableBM25Blob, PortableBM25Index, PortableBM25View};
use triblespace_search::tokens::{hash_tokens, WordHash};

use crate::schemas::blockdag as schema;

/// Archive-text BM25 member mapping, version 3: one document per text
/// payload blob, keyed by its handle.
///
/// Minted with `trible genid` on 2026-10-26:
/// `7FC72BB899FC3D02777E8F0A334255A3`. It replaces version 2
/// (`C896F71EBE5FE1513424B0853F5082B7`), whose documents were Archive nodes,
/// and version 1 (`4EC6991611EF484A37FBD95F6E108FC6`), whose documents were
/// blocks of ordered content parts.
///
/// Changing the selected attribute, term-frequency law, tokenizer behavior,
/// or document/term schemas requires a new mapping id. Joining mapped
/// members belongs to [`PortableBM25Blob`], not this identity. BM25 `k1` /
/// `b` scoring policy is derived query behavior and is deliberately outside
/// the persisted collection identity.
pub const ARCHIVE_TEXT_BM25_MAPPING_V3: Id = id_hex!("7FC72BB899FC3D02777E8F0A334255A3");

pub type ArchiveBM25Index = PortableBM25Index<Handle<UTF8String>, WordHash>;
/// Shared carrier views for one logical Archive search cover.
pub type ArchiveBM25View = PortableBM25View<Handle<UTF8String>, WordHash>;

#[derive(Debug)]
enum DeriveValidation {
    Ready(Blob<PortableBM25Blob>),
    /// A selected text payload is unavailable; the first one, by handle.
    Pending(Inline<Handle<UTF8String>>),
    Rejected(String),
}

/// The archive-text BM25 law, as a describable type.
///
/// A descriptor embeds this rather than only naming it, so a reader holding
/// the pile can learn what the index is without the code that built it.
pub struct ArchiveTextBm25MappingV3;

impl MetaDescribe for ArchiveTextBm25MappingV3 {
    fn describe() -> triblespace::core::trible::Fragment {
        let id: Id = ARCHIVE_TEXT_BM25_MAPPING_V3;
        entity! {
            triblespace::core::id::ExclusiveId::force_ref(&id) @
                metadata::name: "archive-text-bm25-v3",
                metadata::description: "Canonical mapping from one Archive SimpleArchive member to one PortableBM25Blob. Every distinct UTF8String handle the member names under the content-fact payload attribute is one document keyed by that handle, including an empty text; each text is tokenized with hash_tokens and repeated terms contribute exact frequencies. A text's hash fixes its terms, so members join as a plain union. PortableBM25Blob owns target-member validation and join. The k1 and b scoring parameters are deliberately absent because they are query-time behaviour.",
                metadata::tag: metadata::KIND_COLLECTION_MAPPING_ALGORITHM,
        }
    }
}

/// Bound canonical projection from one Archive fact-set member to the
/// portable BM25 image of the texts it names.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArchiveTextBm25Mapping;

impl MapMapping for ArchiveTextBm25Mapping {
    type Target = PortableBM25Blob;

    fn fragment(&self) -> Fragment {
        mapping_fragment()
    }

    fn bind(_parent: &Fragment, attached: &Fragment) -> Result<Self, CollectionOperationError> {
        let actual = triblespace::core::collection::descriptor::mapping_algorithm(attached.facts())
            .map_err(|error| CollectionOperationError::Fatal(error.to_string()))?;
        if actual != Some(ARCHIVE_TEXT_BM25_MAPPING_V3) {
            return Err(CollectionOperationError::Fatal(format!(
                "Archive BM25 mapping algorithm {:?} does not match archive-text-bm25-v3 \
                 {ARCHIVE_TEXT_BM25_MAPPING_V3:X}",
                actual.map(|id| format!("{id:X}")),
            )));
        }
        Ok(Self)
    }

    /// Map one parent node, classifying what stops it by what could still
    /// change the answer. A selected text payload that is not here is a
    /// dependency the store may fetch, after which the map runs again. A
    /// payload whose bytes are not UTF-8 is refused as that node's capacity.
    /// Only a failure to read the store is fatal, because that is the one
    /// thing no other node can get around.
    fn map<R: StoreRead>(
        &self,
        node: &Blob<SimpleArchive>,
        _siblings: &[CollectionData],
        reader: &R,
    ) -> Result<Blob<PortableBM25Blob>, CollectionOperationError> {
        match derive_for_validation(reader, node) {
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
        mapping_algorithm*: <ArchiveTextBm25MappingV3 as MetaDescribe>::describe(),
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
    match derive_for_validation(reader, &source)? {
        DeriveValidation::Ready(blob) => Ok(blob),
        DeriveValidation::Pending(payload) => bail!(
            "Archive BM25 source has a nonresident text payload {}",
            hex::encode_upper(payload.raw)
        ),
        DeriveValidation::Rejected(reason) => bail!("invalid Archive BM25 source: {reason}"),
    }
}

fn derive_for_validation<R>(reader: &R, source: &Blob<SimpleArchive>) -> Result<DeriveValidation>
where
    R: BlobStoreGet,
{
    let handles = match text_payloads(source) {
        Ok(handles) => handles,
        Err(reason) => return Ok(DeriveValidation::Rejected(reason)),
    };

    // Resolve every distinct payload before counting any. Try every one
    // before Pending so malformed bytes cannot hide behind one unavailable
    // payload. Do not test frozen residency first: an acquiring reader can
    // obtain exact text even though this observation correctly keeps
    // reporting it as nonresident. Keep each text as its view, not its
    // tokens: a token is a 32-byte hash, several times the text it came from.
    let mut texts: Vec<(Inline<Handle<UTF8String>>, View<str>)> = Vec::with_capacity(handles.len());
    let mut missing = None;
    for raw in handles {
        let handle: Inline<Handle<UTF8String>> = Inline::new(raw);
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
                    hex::encode_upper(raw),
                )))
            }
        };
        texts.push((handle, text));
    }
    if let Some(payload) = missing {
        return Ok(DeriveValidation::Pending(payload));
    }

    let documents: Vec<Inline<Handle<UTF8String>>> =
        texts.iter().map(|(handle, _)| *handle).collect();
    let mut counts = Vec::new();
    for (document, text) in &texts {
        let mut frequencies: BTreeMap<RawInline, u32> = BTreeMap::new();
        for token in hash_tokens(text.as_ref()) {
            let frequency = frequencies.entry(token.raw).or_default();
            let Some(incremented) = frequency.checked_add(1) else {
                return Ok(DeriveValidation::Rejected(format!(
                    "term frequency overflows u32 for Archive text {}",
                    hex::encode_upper(document.raw)
                )));
            };
            *frequency = incremented;
        }
        counts.extend(
            frequencies
                .into_iter()
                .map(|(term, frequency)| (*document, Inline::<WordHash>::new(term), frequency)),
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

/// The distinct text payload handles a member names, read from its rows in
/// place. A canonical `SimpleArchive` is a sorted run of 64-byte rows
/// (entity 16, attribute 16, value 32); only the attribute is compared.
fn text_payloads(source: &Blob<SimpleArchive>) -> std::result::Result<BTreeSet<RawInline>, String> {
    simplearchive_union::validate_element(source)
        .map_err(|error| format!("source is not a canonical SimpleArchive: {error}"))?;
    let payload = schema::content_fact::payload.id();
    Ok(source
        .bytes
        .as_ref()
        .chunks_exact(TRIBLE_LEN)
        .filter(|row| row[A_START..V_START] == payload[..])
        .map(|row| {
            row[V_START..]
                .try_into()
                .expect("a SimpleArchive row ends in a 32-byte value")
        })
        .collect())
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

    use triblespace::core::blob::TryFromBlob;
    use triblespace::core::inline::encodings::genid::GenId;
    use triblespace::core::inline::IntoInline;
    use triblespace::core::trible::TribleSet;

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

    /// One root node holding the given facts as one simultaneous set.
    fn text_block(facts: &[(Id, &str)]) -> Fragment {
        let mut held = Fragment::empty();
        for (modality, text) in facts {
            held += archive::text_fact(
                *modality,
                schema::content_fact::direction::IN,
                (*text).to_owned(),
            )
            .unwrap();
        }
        archive::block(std::iter::empty::<Id>(), None, held).unwrap()
    }

    fn parse(blob: Blob<PortableBM25Blob>) -> ArchiveBM25Index {
        ArchiveBM25Index::try_from_blob(blob).unwrap()
    }

    fn derive(reader: &PileSnapshot, source: Blob<SimpleArchive>) -> Blob<PortableBM25Blob> {
        derive_element(reader, source).unwrap()
    }

    /// The document key of one text: its blob handle.
    fn text_key(text: &str) -> RawInline {
        let blob: Blob<UTF8String> = text.to_blob();
        blob.get_handle().raw
    }

    fn keys(index: &ArchiveBM25Index) -> BTreeSet<RawInline> {
        index.document_keys().map(|document| document.raw).collect()
    }

    #[test]
    fn a_cold_residual_block_acquires_its_text_without_changing_residency() {
        use triblespace::core::collection::{
            AdmissionPolicy, CollectionPolicy, CollectionSnapshotExt, CollectionStoreExt,
        };
        use triblespace::core::repo::{BlobStoreList, StorageClose};

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cold-archive-bm25.pile");
        File::create(&path).unwrap();
        let signer = ed25519_dalek::SigningKey::from_bytes(&[113; 32]);
        let mut store = crate::storage::open_store_as(&path, signer.verifying_key()).unwrap();
        let root = store
            .collection(
                "cold Archive BM25",
                CollectionPolicy::new(
                    AdmissionPolicy::direct(signer.verifying_key()),
                    AdmissionPolicy::direct(signer.verifying_key()),
                ),
            )
            .unwrap();
        let target = store.attach_with(root, ArchiveTextBm25Mapping).unwrap();
        let text: Blob<UTF8String> = "late comet comet".to_blob();
        let text_handle = text.get_handle();
        let (node, attachments) = source_and_attachments(text_block(&[(
            schema::content_fact::modality::TEXT,
            "late comet comet",
        )]));
        assert!(attachments
            .iter()
            .any(|blob| blob.get_handle().raw == text_handle.raw));
        let facts = TribleSet::try_from_blob(node.clone()).unwrap();
        store.commit(root, &signer, facts.into()).unwrap();
        let frozen = store.snapshot().unwrap();
        assert!(matches!(
            derive_for_validation(&frozen, &node).unwrap(),
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
            selected
                .read_with_acquiring::<ArchiveTextBm25Mapping, ArchiveBM25View>()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            index
                .segments()
                .iter()
                .map(|segment| segment.doc_count())
                .sum::<usize>(),
            1
        );
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
    fn empty_corpus_and_empty_text_remain_documents() {
        let (empty_source, empty_attachments) = source_and_attachments(Fragment::empty());
        let empty_store = StoredBlobs::new(empty_attachments);
        let empty = parse(derive(&empty_store.reader, empty_source));
        assert_eq!(empty.doc_count(), 0);
        assert_eq!(empty.term_count(), 0);

        // An empty text is a document; a node holding only a binary fact,
        // and the content-free bottom, name no text and add none.
        let empty_text = text_block(&[(schema::content_fact::modality::TEXT, "")]);
        let binary_fact = archive::blob_fact(
            schema::content_fact::modality::IMAGE,
            schema::content_fact::direction::IN,
            vec![0, 1, 2, 3],
            "application/octet-stream",
        )
        .unwrap();
        let binary_block = archive::block(std::iter::empty::<Id>(), None, binary_fact).unwrap();
        let bottom = archive::block(std::iter::empty::<Id>(), None, Fragment::empty()).unwrap();
        let mut corpus = empty_text;
        corpus += binary_block;
        corpus += bottom;
        let (source, attachments) = source_and_attachments(corpus);
        let store = StoredBlobs::new(attachments);
        let index = parse(derive(&store.reader, source));
        assert_eq!(index.doc_count(), 1);
        assert_eq!(index.term_count(), 0);
        assert_eq!(keys(&index), BTreeSet::from([text_key("")]));
    }

    #[test]
    fn textless_nodes_do_not_perturb_the_bm25_carrier() {
        let semantic = text_block(&[(schema::content_fact::modality::TEXT, "stable corpus")]);
        let (semantic_source, semantic_attachments) = source_and_attachments(semantic.clone());
        let semantic_store = StoredBlobs::new(semantic_attachments);
        let semantic_index = parse(derive(&semantic_store.reader, semantic_source));

        let mut with_textless = semantic;
        with_textless += archive::block(std::iter::empty::<Id>(), None, Fragment::empty()).unwrap();
        with_textless += archive::block(
            std::iter::empty::<Id>(),
            None,
            archive::blob_fact(
                schema::content_fact::modality::IMAGE,
                schema::content_fact::direction::IN,
                vec![9, 9],
                "application/octet-stream",
            )
            .unwrap(),
        )
        .unwrap();
        let (with_textless_source, with_textless_attachments) =
            source_and_attachments(with_textless);
        let with_textless_store = StoredBlobs::new(with_textless_attachments);
        let with_textless_index = parse(derive(&with_textless_store.reader, with_textless_source));

        assert_eq!(semantic_index, with_textless_index);
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
        assert_eq!(keys(&index), BTreeSet::from([text_key("parent")]));
    }

    /// A text is one document however many nodes and facts name it: two
    /// different nodes (one a root, one after another node) holding the
    /// same text, and one set node holding that text under two modalities,
    /// all index it once, with the frequencies of the text itself. Each
    /// node mapped alone joins to exactly the image of their union.
    #[test]
    fn identical_texts_are_one_document_whoever_holds_them() {
        let root = text_block(&[(schema::content_fact::modality::TEXT, "alpha")]);
        let root_id = root.root().unwrap();
        let held = archive::text_fact(
            schema::content_fact::modality::TEXT,
            schema::content_fact::direction::IN,
            "echo echo quasar",
        )
        .unwrap();
        let first = archive::block(std::iter::empty::<Id>(), None, held.clone()).unwrap();
        let second = archive::block([root_id], None, held).unwrap();
        assert_ne!(first.root(), second.root(), "two different nodes");
        let set = text_block(&[
            (schema::content_fact::modality::THINKING, "echo echo quasar"),
            (
                schema::content_fact::modality::TOOL_RESULT,
                "echo echo quasar",
            ),
        ]);

        let sources: Vec<Blob<SimpleArchive>> = [&root, &first, &second, &set]
            .into_iter()
            .map(|node| node.facts().clone().to_blob())
            .collect();
        let mut union = root;
        union += first;
        union += second;
        union += set;
        let (union_source, attachments) = source_and_attachments(union);
        let store = StoredBlobs::new(attachments);
        let index = parse(derive(&store.reader, union_source));
        assert_eq!(
            keys(&index),
            BTreeSet::from([text_key("alpha"), text_key("echo echo quasar")])
        );
        let document: Inline<Handle<UTF8String>> = Inline::new(text_key("echo echo quasar"));
        let echo = hash_tokens("echo")[0];
        assert_eq!(index.term_frequency(&document, &echo).unwrap(), 2);
        assert_eq!(
            index
                .query()
                .unwrap()
                .query_multi(&hash_tokens("quasar"))
                .unwrap()
                .len(),
            1
        );

        let alone: Vec<ArchiveBM25Index> = sources
            .into_iter()
            .map(|source| parse(derive(&store.reader, source)))
            .collect();
        for image in &alone[1..] {
            assert_eq!(keys(image), BTreeSet::from([text_key("echo echo quasar")]));
        }
        assert_eq!(ArchiveBM25Index::merge(alone.iter()).unwrap(), index);
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
        let fact_id = fact.root().unwrap();
        let block_id = Id::new([0xA5; 16]).unwrap();
        let mut graph_with_unknown_fact = fact;
        graph_with_unknown_fact += entity! { ExclusiveId::force_ref(&block_id) @
            metadata::tag: &schema::block::KIND,
            metadata::name: "unexpected node field",
            schema::block::holds: &fact_id,
        };
        let (graph_with_unknown_fact, attachments) =
            source_and_attachments(graph_with_unknown_fact);
        let open_world_store = StoredBlobs::new(attachments);
        let index = parse(derive(&open_world_store.reader, graph_with_unknown_fact));
        assert_eq!(
            keys(&index),
            BTreeSet::from([text_key("open world")]),
            "unknown facts and opaque node ids do not close the schema"
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

    /// One Archive node cut in two: its own facts (tag and `holds`), and its
    /// held facts with every text payload.
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
            .unwrap()
            .into_iter()
            .map(|(document, score)| (document.raw, (f64::from(score) * 1e4).round() as i64))
            .collect()
    }

    /// The cover-query equivalence law for the archive-text mapping, over
    /// random commits (some of them an Archive node's own facts in one
    /// commit and its held facts in another), the host's carry or not,
    /// random host merges over possibly overlapping nodes, and an attachment
    /// at random nodes: the production read -- the attached cover and the
    /// residual built in memory -- scores every query exactly as the index of
    /// the union of every foundation it reads. A split node is no obstacle,
    /// because the documents are texts and every member maps, so nothing is
    /// ever unread.
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
        let mut attachments_taken = 0;
        let mut host_merges = 0;
        for seed in 0..24u64 {
            let mut random = Stream(seed.wrapping_mul(0x2545_F491_4F6C_DD1D) + 7);
            let mut store = MemoryRepo::for_host(host);
            let policy =
                CollectionPolicy::new(AdmissionPolicy::direct(host), AdmissionPolicy::direct(host));
            let root = store
                .collection(&format!("archive-law-{seed}"), policy)
                .unwrap();
            let target = store.attach_with(root, ArchiveTextBm25Mapping).unwrap();
            // Enough commits that a host merge can name eight distinct nodes:
            // the collection algebra accepts exactly eight-way MERGEs.
            for _ in 0..8 + random.below(8) {
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
            // Host merges over eight random, possibly overlapping, nodes: some
            // hold both halves of a split block, some only one.
            for _ in 0..random.below(5) {
                let lattice = nodes(&mut store);
                if lattice.len() < 8 {
                    break;
                }
                let mut picked: BTreeSet<CollectionData> = BTreeSet::new();
                while picked.len() < 8 {
                    picked.insert(lattice[random.below(lattice.len() as u64) as usize]);
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
                host_merges += 1;
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
                let image = ArchiveTextBm25Mapping.map(&bytes, &[], &snapshot);
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
                .read_with::<ArchiveTextBm25Mapping, ArchiveBM25View>()
                .unwrap();
            assert!(read.unread().is_empty(), "seed {seed}");
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
            let expected = ArchiveTextBm25Mapping
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
        // The seeds exercise attachments taken as well as residuals.
        assert!(attachments_taken > 0);
        assert!(host_merges > 0, "no seed reached an eight-way host merge");
    }

    // The three tests below spread one node's held facts over members.
    // Faculties' importers never write that, but another writer can. Under
    // the node-document mapping (version 2) the cover law failed here, since
    // a node's frequency summed over facts no single member held; with texts
    // as documents each member's image is exact and the law holds.

    /// One set node of several distinct text facts, and each fact. The
    /// modality varies with the position so equal words stay distinct
    /// facts: a set of equal facts is one fact.
    fn parts_block(texts: &[&str]) -> (Fragment, Vec<Fragment>) {
        const MODALITIES: [Id; 3] = [
            schema::content_fact::modality::TEXT,
            schema::content_fact::modality::THINKING,
            schema::content_fact::modality::TOOL_RESULT,
        ];
        let mut facts = Vec::new();
        let mut held = Fragment::empty();
        for (index, text) in texts.iter().enumerate() {
            let fact = archive::text_fact(
                MODALITIES[index % MODALITIES.len()],
                schema::content_fact::direction::IN,
                (*text).to_owned(),
            )
            .unwrap();
            facts.push(fact.clone());
            held += fact;
        }
        let node = archive::block(std::iter::empty::<Id>(), None, held).unwrap();
        (node, facts)
    }

    /// The facts of `node` a member holds when it has the `holds` fact and
    /// the closure of each of `facts`, and, when `tagged`, every other fact
    /// of the node itself.
    fn block_piece(node: &Fragment, facts: &[&Fragment], tagged: bool) -> Fragment {
        let node_id = node.root().unwrap();
        let holds = schema::block::holds.id();
        let mut piece = TribleSet::new();
        for fact in facts {
            piece += fact.facts().clone();
            let fact_id: Inline<GenId> = fact.root().unwrap().to_inline();
            for trible in node.facts().iter() {
                if trible.e() == &node_id
                    && trible.a() == &holds
                    && trible.v::<GenId>().raw == fact_id.raw
                {
                    piece.insert(trible);
                }
            }
        }
        if tagged {
            for trible in node.facts().iter() {
                if trible.e() == &node_id && trible.a() != &holds {
                    piece.insert(trible);
                }
            }
        }
        Fragment::from(piece)
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
        let target = store.attach_with(root, ArchiveTextBm25Mapping).unwrap();
        for blob in texts {
            store.put::<UnknownBlob, _>(blob).unwrap();
        }
        let mut union = TribleSet::new();
        for node in nodes {
            union += node.facts().clone();
            store.commit(root, &key, node.clone()).unwrap();
        }
        pollster::block_on(store.ensure_attached_with::<ArchiveTextBm25Mapping>(target, &key))
            .unwrap();

        let snapshot = store.snapshot().unwrap();
        let attached = snapshot.attached(target).unwrap();
        assert!(attached.residual().is_empty(), "every node is attached");
        let read = attached
            .read_with::<ArchiveTextBm25Mapping, ArchiveBM25View>()
            .unwrap();
        assert!(read.unread().is_empty());
        let expected = ArchiveTextBm25Mapping
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

    /// Two members each hold the set node and one of its two facts, each
    /// fact with the same word. Both facts name one text, so each member and
    /// the union index that one text with a frequency of one.
    #[test]
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

    /// The same set node, one member holding it with its first fact and
    /// another holding only its `holds` fact for the second fact and that
    /// fact's closure, not the node's tag. The second member still names the
    /// text, and the text is whole wherever it is named.
    #[test]
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

    /// The cover-query equivalence law over set nodes whose held facts are
    /// spread over members: whole, one fact per member with the node's own
    /// facts in each, two overlapping runs of facts, or the node's own facts
    /// with its first fact and each other fact's `holds` fact and closure
    /// elsewhere; the pieces of different nodes share members at random.
    /// Every member maps, so the law must hold with nothing unread.
    #[test]
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
