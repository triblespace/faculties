//! Bounded, passive inspection of schema-described attachment references.
//!
//! This is an audit, not a reader admission rule. Unknown interpretations leave
//! ordinary facts usable, but prevent this audit from claiming complete closure.
//! In particular, a resident-only conservative child walk cannot find absence.

use std::collections::BTreeSet;

use serde_json::{json, Value};
use triblespace::core::metadata::{self, MetaDescribe};
use triblespace::core::repo::pile::{GetBlobError, PileFileSnapshot};
use triblespace::prelude::blobencodings::{RawBytes, SimpleArchive, UTF8String};
use triblespace::prelude::inlineencodings::{
    Blake3, Boolean, GenId, Handle, Hash, NsTAIInterval, ShortString, I256BE, U256BE,
};
use triblespace::prelude::*;

const MAX_REFERENCES: usize = 100_000;
const MAX_BYTES: usize = 256 * 1024 * 1024;

pub fn inspect(snapshot: &PileFileSnapshot, facts: &TribleSet, descriptions: &TribleSet) -> Value {
    // All scratch belongs to this finite audit. No catalogue or receipt is
    // retained between observations, and no data is written to the source.
    let mut queue = vec![facts.clone()];
    let mut visited = BTreeSet::new();
    let mut unsupported = BTreeSet::new();
    let mut references = Vec::new();
    let mut bytes_read = 0usize;
    let mut limited = false;
    // Attribute metadata names its encoding but need not carry that encoding's
    // description. Supply only the interpretations this binary actually knows;
    // this is interpreter vocabulary, not recovered source evidence. Retain all
    // supplied interpretations too, rather than choosing one on conflict.
    let mut vocabulary = descriptions.clone();
    vocabulary += metadata::describe().facts().clone();
    vocabulary += Handle::<RawBytes>::describe().facts().clone();
    vocabulary += Handle::<UTF8String>::describe().facts().clone();
    vocabulary += Handle::<SimpleArchive>::describe().facts().clone();
    let descriptions = &vocabulary;
    let inline_leaves = [
        GenId::id(),
        ShortString::id(),
        Boolean::id(),
        I256BE::id(),
        U256BE::id(),
        NsTAIInterval::id(),
        Hash::<Blake3>::id(),
    ];

    while let Some(frame) = queue.pop() {
        // Query every used attribute, including ones without any decodable
        // schema. A positive reference join alone would silently omit them.
        let attributes: BTreeSet<Id> = frame.iter().map(|fact| *fact.a()).collect();
        for attribute in attributes {
            let encodings: BTreeSet<Id> = find!(encoding: Id,
                pattern!(descriptions, [{ attribute @ metadata::value_encoding: ?encoding }])
            )
            .collect();
            if encodings.is_empty() {
                unsupported.insert(format!(
                    "attribute {attribute:x}: no understood value encoding"
                ));
            }
            for encoding in encodings {
                if inline_leaves.contains(&encoding) {
                    continue;
                }
                let known_handle = exists!(
                    (blob: Id),
                    pattern!(descriptions, [{
                        encoding @ metadata::blob_encoding: ?blob,
                        metadata::hash_schema: Blake3::id(),
                    }])
                );
                if !known_handle {
                    unsupported.insert(format!(
                        "attribute {attribute:x}: inline encoding {encoding:x} not understood"
                    ));
                }
                for hash in find!(hash: Id,
                    pattern!(descriptions, [{ encoding @ metadata::hash_schema: ?hash }])
                ) {
                    if hash != Blake3::id() {
                        unsupported.insert(format!(
                            "attribute {attribute:x}: hash interpretation {hash:x} not understood"
                        ));
                    }
                }
            }
        }

        for (attribute, value, encoding) in find!(
            (attribute: Id, value: Inline<UnknownInline>, encoding: Id),
            temp!((inline), and!(
                pattern!(&frame, [{ _?subject @ ?attribute: ?value }]),
                pattern!(descriptions, [
                    { ?attribute @ metadata::value_encoding: ?inline },
                    { ?inline @ metadata::blob_encoding: ?encoding,
                        metadata::hash_schema: Blake3::id() },
                ]),
            ))
        ) {
            if !visited.insert((value.raw, encoding)) {
                continue;
            }
            if visited.len() > MAX_REFERENCES || bytes_read >= MAX_BYTES {
                limited = true;
                break;
            }
            let handle: Inline<Handle<RawBytes>> = value.transmute();
            let mut row = json!({
                "attribute": format!("{attribute:x}"),
                "handle": hex::encode(value.raw),
                "blob_encoding": format!("{encoding:x}"),
            });
            let blob: Blob<RawBytes> = match snapshot.get(handle) {
                Ok(blob) => blob,
                Err(error) => {
                    // PileFile's metadata probe conflates hash-invalid and
                    // missing blobs. Inspect the actual get error instead.
                    row["status"] = json!(match &error {
                        GetBlobError::BlobNotFound(_) => "absent",
                        GetBlobError::ValidationError(_) => "hash_validation_failed",
                        GetBlobError::ConversionError(_) => "read_conversion_failed",
                    });
                    row["detail"] = json!(error.to_string());
                    references.push(row);
                    continue;
                }
            };
            row["bytes"] = json!(blob.bytes.len());
            if blob.bytes.len() > MAX_BYTES.saturating_sub(bytes_read) {
                row["status"] = json!("byte_limit_reached");
                references.push(row);
                limited = true;
                break;
            }
            bytes_read += blob.bytes.len();
            // BlobStoreGet validates content identity. The processing budget
            // below cannot bound the store's validation I/O before it returns.
            if encoding == RawBytes::id() {
                // RawBytes positively specifies opaque bytes, not an archive
                // whose arbitrary aligned words should be treated as edges.
                row["status"] = json!("readable_opaque_leaf");
            } else if encoding == UTF8String::id() {
                match std::str::from_utf8(&blob.bytes) {
                    Ok(_) => row["status"] = json!("readable_text_leaf"),
                    Err(error) => {
                        row["status"] = json!("decode_failed");
                        row["detail"] = json!(error.to_string());
                    }
                }
            } else if encoding == SimpleArchive::id() {
                match TribleSet::try_from_blob(blob.transmute::<SimpleArchive>()) {
                    Ok(child) => {
                        row["status"] = json!("readable_archive");
                        queue.push(child);
                    }
                    Err(error) => {
                        row["status"] = json!("decode_failed");
                        row["detail"] = json!(error.to_string());
                    }
                }
            } else {
                row["status"] = json!("resident_unsupported_encoding");
                unsupported.insert(format!(
                    "blob encoding {encoding:x}: children not understood"
                ));
            }
            references.push(row);
        }
        if limited {
            break;
        }
    }
    json!({
        "scope": "unique (digest, encoding) references in supplied facts, first attribute shown; not whole-pile or application completeness",
        "interpretation_scope": "supplied descriptions plus this binary's metadata vocabulary and Handle<RawBytes/UTF8String/SimpleArchive> descriptions; not proof of source self-description",
        "references": references,
        "unsupported_interpretations": unsupported,
        "limit_reached": limited,
        "bytes_read": bytes_read,
        "max_references": MAX_REFERENCES,
        "max_bytes": MAX_BYTES,
        "budget_scope": "attachment payload processing, excluding store validation I/O and root archive inspection",
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;
    use triblespace::core::repo::pile::PileFile;

    fn store() -> (NamedTempFile, PileFile) {
        let file = NamedTempFile::new().unwrap();
        let pile = PileFile::open(file.path()).unwrap();
        (file, pile)
    }

    #[test]
    fn missing_text_reference_is_not_a_clean_empty_walk() {
        let (_file, mut store) = store();
        let fragment = entity! { metadata::description: "not stored" };
        let snapshot = store.snapshot().unwrap();
        let report = inspect(&snapshot, fragment.facts(), fragment.metafacts());
        assert_eq!(report["references"].as_array().unwrap().len(), 1);
        assert_eq!(report["references"][0]["status"], "absent");
    }

    #[test]
    fn readable_reference_and_unknown_attribute_are_reported_independently() {
        let (_file, mut store) = store();
        let text = store.put::<UTF8String, _>("resident".to_string()).unwrap();
        let mut fragment = entity! { metadata::description: text };
        let unknown = fucid();
        let subject = fucid();
        let fact = Trible::new(&subject, &unknown.id, &text.transmute::<UnknownInline>());
        fragment += TribleSet::from_iter([fact]);
        let snapshot = store.snapshot().unwrap();
        let report = inspect(&snapshot, fragment.facts(), fragment.metafacts());
        assert_eq!(report["references"][0]["status"], "readable_text_leaf");
        assert_eq!(
            report["unsupported_interpretations"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn malformed_utf8_is_not_reported_as_readable() {
        let (_file, mut store) = store();
        let bytes = store.put::<RawBytes, _>(vec![255u8]).unwrap();
        let fragment = entity! { metadata::description: bytes.transmute::<Handle<UTF8String>>() };
        let snapshot = store.snapshot().unwrap();
        let report = inspect(&snapshot, fragment.facts(), fragment.metafacts());
        assert_eq!(report["references"][0]["status"], "decode_failed");
    }

    #[test]
    fn resident_unknown_format_does_not_imply_known_children() {
        use faculties::schemas::embeddings::{attr, Embedding768};

        let (_file, mut store) = store();
        let handle = store.put::<Embedding768, _>(vec![0.0f32; 768]).unwrap();
        let mut fragment = entity! { attr::embedding: handle };
        fragment.describe_with(Handle::<Embedding768>::describe());
        let snapshot = store.snapshot().unwrap();
        let report = inspect(&snapshot, fragment.facts(), fragment.metafacts());
        assert_eq!(
            report["references"][0]["status"],
            "resident_unsupported_encoding"
        );
        assert!(!report["unsupported_interpretations"]
            .as_array()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn archive_walk_names_a_missing_nested_reference() {
        let (_file, mut store) = store();
        let child = entity! { metadata::description: "missing child text" };
        let archive = store.put::<SimpleArchive, _>(child.facts()).unwrap();
        let parent = entity! { metadata::archive: archive };
        let mut descriptions = parent.metafacts().clone();
        descriptions += child.metafacts().clone();
        let snapshot = store.snapshot().unwrap();
        let report = inspect(&snapshot, parent.facts(), &descriptions);
        let rows = report["references"].as_array().unwrap();
        assert!(rows.iter().any(|row| row["status"] == "readable_archive"));
        assert!(rows.iter().any(|row| row["status"] == "absent"));
    }
}
