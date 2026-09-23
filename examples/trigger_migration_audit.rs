//! Passive, immediate-payload evidence for explicitly named collection roots.
//!
//! Run an already-built binary with `--pile /exact/existing.pile --collection
//! blake3:<descriptor> [--collection blake3:<descriptor> ...]`. There are no
//! environment defaults, signer, network store, registration or repair calls.
//! `PileFile::open` requires an existing append-capable file descriptor, but
//! this path only takes ONE passive snapshot and closes without dirty writes.
//! It deliberately avoids `Pile`'s whole-pile coverage settlement.
//! Use an isolated complete copy, never live custody: replay holds a shared
//! file lock and can delay writers even though the audit appends nothing.
//!
//! JSON lines preserve record identity, signature, admission and immediate
//! archive observations separately. Exit success means the audit ran, NOT that
//! migration, recursive closure, or application projections have been proved.
//! Known records form a fingerprint set, not a physical duplicate-frame census.
//! Unknown record formats and recursive/application semantics remain unproved.
//! No payload prose, scripts or private keys are printed.

#[path = "trigger_migration_audit/attachments.rs"]
mod attachments;

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use ed25519_dalek::VerifyingKey;
use serde_json::{json, Value};
use triblespace::core::blob::encodings::simplearchive::SimpleArchive;
use triblespace::core::blob::{Blob, TryFromBlob};
use triblespace::core::collection::{
    AdmissionPolicy, Collection, CollectionHandle, CollectionRead, CollectionRecord,
    CollectionRecordSelector,
};
use triblespace::core::inline::encodings::hash::Handle;
use triblespace::core::inline::Inline;
use triblespace::core::repo::pile::{GetBlobError, PileFile, PileFileSnapshot};
use triblespace::core::repo::{BlobStoreGet, SnapshotSource};
use triblespace::core::trible::TribleSet;

#[derive(Parser)]
#[command(about = "Audit explicit collection claims and immediate archives without writing")]
struct Args {
    /// Existing offline pile copy, never live custody (replay holds a shared lock).
    /// No PILE or other environment fallback is consulted.
    #[arg(long)]
    pile: PathBuf,
    /// Exact descriptor handle, repeated for every source or target to inspect.
    #[arg(long = "collection", required = true, value_parser = parse_handle)]
    collections: Vec<CollectionHandle>,
}

fn parse_handle(text: &str) -> std::result::Result<CollectionHandle, String> {
    let text = text.strip_prefix("blake3:").unwrap_or(text);
    let mut raw = [0; 32];
    hex::decode_to_slice(text, &mut raw)
        .map_err(|_| "expected an exact 64-hex collection descriptor handle".to_owned())?;
    Ok(CollectionHandle::new(raw))
}

fn emit(output: &mut impl Write, evidence: Value) -> Result<()> {
    serde_json::to_writer(&mut *output, &evidence)?;
    output.write_all(b"\n")?;
    Ok(())
}

fn policy_evidence(policy: &AdmissionPolicy) -> Value {
    match policy {
        AdmissionPolicy::Open => json!({ "kind": "open" }),
        AdmissionPolicy::Quorum(quorum) => json!({
            "kind": "quorum",
            "roots": quorum.roots().iter()
                .map(|root| hex::encode(root.to_bytes())).collect::<Vec<_>>(),
            "invoke_threshold": quorum.invoke_threshold(),
            "legacy_delegate_threshold": quorum.delegate_threshold(),
        }),
    }
}

// Operation-local decoded archive result, never retained across observations.
// TribleSet is the native fact relation; no application entities are rebuilt.
struct ArchiveObservation {
    evidence: Value,
    facts: Option<TribleSet>,
}

fn inspect_archive(snapshot: &PileFileSnapshot, raw: [u8; 32]) -> ArchiveObservation {
    let handle = Inline::<Handle<SimpleArchive>>::new(raw);
    let blob = match snapshot.get::<Blob<SimpleArchive>, SimpleArchive>(handle) {
        Ok(blob) => blob,
        Err(error) => {
            let status = match &error {
                GetBlobError::BlobNotFound(_) => "absent",
                GetBlobError::ValidationError(_) => "hash_validation_failed",
                GetBlobError::ConversionError(_) => "read_conversion_failed",
            };
            // Debug formatting ValidationError would disclose the blob bytes.
            return ArchiveObservation {
                evidence: json!({ "status": status, "error": error.to_string() }),
                facts: None,
            };
        }
    };
    let bytes = blob.bytes.len();
    match TribleSet::try_from_blob(blob) {
        Ok(facts) => ArchiveObservation {
            evidence: json!({
                "status": "canonical_simplearchive",
                "bytes": bytes,
                "tribles": facts.len(),
            }),
            facts: Some(facts),
        },
        Err(error) => ArchiveObservation {
            evidence: json!({
                "status": "not_canonical_simplearchive",
                "bytes": bytes,
                "error": error.to_string(),
            }),
            facts: None,
        },
    }
}

fn facts_digest(facts: &TribleSet) -> String {
    let mut digest = blake3::Hasher::new();
    for trible in facts.iter_ordered() {
        digest.update(&trible.data);
    }
    digest.finalize().to_hex().to_string()
}

fn audit_snapshot(
    snapshot: &PileFileSnapshot,
    handles: &[CollectionHandle],
    output: &mut impl Write,
) -> Result<()> {
    // All maps and sets here are scratch for this one frozen audit operation.
    // Nothing is a retained catalog or used to decide application semantics.
    let selected: BTreeSet<_> = handles.iter().copied().collect();
    let mut archives: BTreeMap<[u8; 32], ArchiveObservation> = BTreeMap::new();
    for handle in selected {
        let collection_id = hex::encode(handle.raw);
        let descriptor = inspect_archive(snapshot, handle.raw);
        let opened =
            Collection::<SimpleArchive>::open(snapshot, handle).map_err(|error| error.to_string());
        let policy = match &opened {
            Ok(collection) => match collection.policy(snapshot) {
                Ok(policy) => json!({
                    "status": "readable",
                    "read": policy_evidence(policy.read()),
                    "write": policy_evidence(policy.write()),
                }),
                Err(error) => json!({ "status": "unavailable", "error": error.to_string() }),
            },
            Err(error) => json!({ "status": "unavailable", "error": error }),
        };
        emit(
            output,
            json!({
                "kind": "collection_descriptor",
                "collection": collection_id,
                "archive": descriptor.evidence,
                "typed_open": match &opened {
                    Ok(_) => json!({ "status": "understood_simplearchive" }),
                    Err(error) => json!({ "status": "unavailable", "error": error }),
                },
                "policy": policy,
            }),
        )?;

        let selectors = BTreeSet::from([CollectionRecordSelector::Collection(handle)]);
        let records = snapshot
            .select_records(&selectors)
            .context("read explicitly selected collection records")?;
        let mut admissions: BTreeMap<[u8; 32], Value> = BTreeMap::new();
        let mut record_ids = BTreeSet::new();
        let mut raw_pairs = BTreeSet::new();
        let mut valid_pairs = BTreeSet::new();
        let mut admitted_pairs = BTreeSet::new();
        let mut denied_pairs = BTreeSet::new();
        let mut unresolved_pairs = BTreeSet::new();
        let mut readable_admitted_pairs = BTreeSet::new();
        let mut admitted_facts = TribleSet::new();
        let mut admitted_metadata = TribleSet::new();
        let mut commits = 0;
        let mut invalid_signatures = 0;
        let mut other_records = 0;
        for record in records {
            if !record_ids.insert(record.fingerprint().raw()) {
                continue;
            }
            let fingerprint = format!("{:X}", record.fingerprint());
            let signature = record.verify_strict();
            let author = record.public_key().raw;
            let commit = match record {
                CollectionRecord::Commit(commit) => commit,
                other => {
                    other_records += 1;
                    emit(
                        output,
                        json!({
                            "kind": "other_known_record",
                            "collection": collection_id,
                            "fingerprint": fingerprint,
                            "record_kind": match other {
                                CollectionRecord::Merge(_) => "merge",
                                CollectionRecord::Derive(_) => "derive",
                                CollectionRecord::Commit(_) => unreachable!(),
                            },
                            "author": hex::encode(author),
                            "signature_valid": signature.is_ok(),
                            "signature_error": signature.err().map(|error| error.to_string()),
                            "content_carry": "not_a_commit",
                        }),
                    )?;
                    continue;
                }
            };
            commits += 1;
            let valid = signature.is_ok();
            if !valid {
                invalid_signatures += 1;
            }
            let admission = admissions.entry(author).or_insert_with(|| {
                let collection = match &opened {
                    Ok(collection) => collection,
                    Err(error) => return json!({ "status": "unavailable", "error": error }),
                };
                let key = match VerifyingKey::from_bytes(&author) {
                    Ok(key) => key,
                    Err(error) => {
                        return json!({ "status": "unavailable", "error": error.to_string() });
                    }
                };
                match collection.writer_is_admitted(snapshot, key) {
                    Ok(true) => json!({ "status": "admitted" }),
                    // A policy diagnostic failure must not become an apparent
                    // negative authority proof merely because no rule matched.
                    Ok(false) if policy["status"] != "readable" => json!({
                        "status": "unavailable", "error": "policy is not readable",
                    }),
                    Ok(false) => json!({ "status": "not_admitted" }),
                    Err(error) => json!({ "status": "unavailable", "error": error.to_string() }),
                }
            });
            let pair = (commit.data().raw, commit.metadata().raw);
            raw_pairs.insert(pair);
            if valid {
                valid_pairs.insert(pair);
                match admission["status"].as_str() {
                    Some("admitted") => {
                        admitted_pairs.insert(pair);
                    }
                    Some("not_admitted") => {
                        denied_pairs.insert(pair);
                    }
                    _ => {
                        unresolved_pairs.insert(pair);
                    }
                }
            }
            for raw in [pair.0, pair.1] {
                if let std::collections::btree_map::Entry::Vacant(entry) = archives.entry(raw) {
                    let observation = inspect_archive(snapshot, raw);
                    emit(
                        output,
                        json!({
                            "kind": "archive",
                            "handle": hex::encode(raw),
                            "interpretation": "structural SimpleArchive probe; descriptor compatibility is separate",
                            "observation": observation.evidence,
                        }),
                    )?;
                    entry.insert(observation);
                }
            }
            let data = &archives[&pair.0];
            let metadata = &archives[&pair.1];
            if valid && admission["status"] == "admitted" {
                if let Some(facts) = &data.facts {
                    admitted_facts += facts.clone();
                }
                if let Some(facts) = &metadata.facts {
                    admitted_metadata += facts.clone();
                }
                if data.facts.is_some() && metadata.facts.is_some() {
                    readable_admitted_pairs.insert(pair);
                }
            }
            let (signature_r, signature_s) = commit.signature();
            emit(
                output,
                json!({
                    "kind": "commit",
                    "collection": collection_id,
                    "fingerprint": fingerprint,
                    "data": hex::encode(pair.0),
                    "metadata": hex::encode(pair.1),
                    "author": hex::encode(author),
                    "signature_r": hex::encode(signature_r.raw),
                    "signature_s": hex::encode(signature_s.raw),
                    "signature_valid": valid,
                    "signature_error": signature.err().map(|error| error.to_string()),
                    "writer_admission": admission,
                    "asserts_admitted_pair": valid && admission["status"] == "admitted",
                    "data_status": data.evidence["status"],
                    "metadata_status": metadata.evidence["status"],
                }),
            )?;
        }
        let mut attachment_roots = admitted_facts.clone();
        attachment_roots += admitted_metadata.clone();
        let mut record_digest = blake3::Hasher::new();
        for id in &record_ids {
            record_digest.update(id);
        }
        emit(
            output,
            json!({
                "kind": "attachment_observation",
                "collection": collection_id,
                "root_scope": "decoded admitted data AND metafact archives; may be partial",
                "observation": attachments::inspect(snapshot, &attachment_roots, &admitted_metadata),
            }),
        )?;
        emit(
            output,
            json!({
                "kind": "collection_summary",
                "collection": collection_id,
                "known_commits": commits,
                "invalid_commit_signatures": invalid_signatures,
            "other_known_records": other_records,
            "known_record_set_blake3": record_digest.finalize().to_hex().to_string(),
                "raw_pairs": raw_pairs.len(),
                "valid_signed_pairs": valid_pairs.len(),
                "admitted_pairs": admitted_pairs.len(),
                "valid_pairs_not_proved_admitted": valid_pairs.difference(&admitted_pairs).count(),
                "valid_pairs_with_unresolved_admission": unresolved_pairs.difference(&admitted_pairs).count(),
                "valid_pairs_only_denied": denied_pairs.iter()
                    .filter(|pair| !admitted_pairs.contains(*pair) && !unresolved_pairs.contains(*pair)).count(),
                "readable_admitted_pairs": readable_admitted_pairs.len(),
                "admitted_pairs_with_unreadable_archives": admitted_pairs.difference(&readable_admitted_pairs).count(),
                "decoded_admitted_data_tribles": admitted_facts.len(),
                "decoded_admitted_data_blake3": facts_digest(&admitted_facts),
                "decoded_admitted_metadata_tribles": admitted_metadata.len(),
                "decoded_admitted_metadata_blake3": facts_digest(&admitted_metadata),
                "fact_union_scope": "successfully decoded admitted COMMIT archives only; may be partial",
                "recursive_closure": "unproved",
                "application_projection": "unproved",
            }),
        )?;
    }
    Ok(())
}

fn audit(path: &Path, handles: &[CollectionHandle], output: &mut impl Write) -> Result<()> {
    if handles.is_empty() {
        bail!("at least one explicit collection handle is required");
    }
    let mut pile = PileFile::open(path).context("open existing pile for passive audit")?;
    let result = (|| {
        let snapshot = pile
            .snapshot()
            .context("freeze one passive pile snapshot")?;
        emit(
            output,
            json!({
                "kind": "audit_scope",
                "pile": path,
                "collections": handles.iter().map(|handle| hex::encode(handle.raw)).collect::<Vec<_>>(),
                "observation": "one native PileFileSnapshot",
                "record_scope": "known COMMIT/MERGE/DERIVE fingerprint set, not physical frames",
                "opaque_record_coverage": "unproved",
                "recursive_closure": "unproved",
                "application_projection": "unproved",
                "writes": "none",
            }),
        )?;
        audit_snapshot(&snapshot, handles, output)?;
        emit(
            output,
            json!({
                "kind": "audit_finished",
                "meaning": "requested observations emitted, not migration or closure certification",
            }),
        )
    })();
    let close = pile.close();
    match (result, close) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(anyhow!("close passive pile: {error}")),
        (Err(error), Err(close_error)) => {
            Err(error.context(format!("close also failed: {close_error}")))
        }
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    let stdout = std::io::stdout();
    let mut output = BufWriter::new(stdout.lock());
    let result = audit(&args.pile, &args.collections, &mut output);
    output.flush().context("flush audit evidence")?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use tempfile::NamedTempFile;
    use triblespace::core::blob::encodings::rawbytes::RawBytes;
    use triblespace::core::collection::{
        CollectionCommit, CollectionPolicy, CollectionStore, CollectionStoreExt,
    };
    use triblespace::core::metadata;
    use triblespace::core::repo::BlobStorePut;
    use triblespace::prelude::entity;

    fn fixture(
        populate: impl FnOnce(&mut PileFile, Collection<SimpleArchive>, &SigningKey),
    ) -> (NamedTempFile, CollectionHandle) {
        let file = NamedTempFile::new().unwrap();
        let mut pile = PileFile::open(file.path()).unwrap();
        let key = SigningKey::from_bytes(&[31; 32]);
        let policy = CollectionPolicy::new(
            AdmissionPolicy::direct(key.verifying_key()),
            AdmissionPolicy::direct(key.verifying_key()),
        );
        let collection = pile.collection("audit fixture", policy).unwrap();
        populate(&mut pile, collection, &key);
        pile.close().unwrap();
        (file, collection.handle())
    }

    fn evidence(file: &NamedTempFile, handle: CollectionHandle) -> Vec<Value> {
        let before = file.as_file().metadata().unwrap().len();
        let mut output = Vec::new();
        audit(file.path(), &[handle], &mut output).unwrap();
        assert_eq!(
            file.as_file().metadata().unwrap().len(),
            before,
            "audit appended bytes"
        );
        String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn summary(rows: &[Value]) -> &Value {
        rows.iter()
            .find(|row| row["kind"] == "collection_summary")
            .unwrap()
    }

    #[test]
    fn valid_unadmitted_commit_is_not_admitted_or_decoded_as_admitted_facts() {
        let (file, handle) = fixture(|pile, collection, _| {
            let other = SigningKey::from_bytes(&[32; 32]);
            pile.commit(
                collection,
                &other,
                entity! { metadata::tag: metadata::KIND_MULTI },
            )
            .unwrap();
        });
        let rows = evidence(&file, handle);
        let commit = rows.iter().find(|row| row["kind"] == "commit").unwrap();
        assert_eq!(commit["signature_valid"], true);
        assert_eq!(commit["writer_admission"]["status"], "not_admitted");
        assert_eq!(summary(&rows)["raw_pairs"], 1);
        assert_eq!(summary(&rows)["valid_signed_pairs"], 1);
        assert_eq!(summary(&rows)["admitted_pairs"], 0);
        assert_eq!(summary(&rows)["valid_pairs_only_denied"], 1);
        assert_eq!(summary(&rows)["decoded_admitted_data_tribles"], 0);
    }

    #[test]
    fn absent_data_and_metadata_are_not_clean_empty_archives() {
        let (file, handle) = fixture(|pile, collection, key| {
            let commit = CollectionCommit::sign(
                key,
                collection.handle(),
                Inline::new([91; 32]),
                Inline::new([92; 32]),
            );
            pile.insert(CollectionRecord::Commit(commit)).unwrap();
        });
        let rows = evidence(&file, handle);
        let commit = rows.iter().find(|row| row["kind"] == "commit").unwrap();
        assert_eq!(commit["data_status"], "absent");
        assert_eq!(commit["metadata_status"], "absent");
        assert_eq!(summary(&rows)["admitted_pairs"], 1);
        assert_eq!(summary(&rows)["readable_admitted_pairs"], 0);
        assert_eq!(summary(&rows)["admitted_pairs_with_unreadable_archives"], 1);
    }

    #[test]
    fn resident_malformed_archive_is_a_decode_failure_not_absence() {
        let (file, handle) = fixture(|pile, collection, key| {
            let malformed = pile.put::<RawBytes, _>(vec![1u8; 65]).unwrap();
            let metadata = pile.put::<SimpleArchive, _>(TribleSet::new()).unwrap();
            let commit = CollectionCommit::sign(
                key,
                collection.handle(),
                Inline::new(malformed.raw),
                metadata,
            );
            pile.insert(CollectionRecord::Commit(commit)).unwrap();
        });
        let rows = evidence(&file, handle);
        let commit = rows.iter().find(|row| row["kind"] == "commit").unwrap();
        assert_eq!(commit["data_status"], "not_canonical_simplearchive");
        assert_eq!(commit["metadata_status"], "canonical_simplearchive");
        assert_eq!(summary(&rows)["admitted_pairs_with_unreadable_archives"], 1);
    }

    #[test]
    fn extra_multivalued_facts_remain_readable_without_domain_validation() {
        let (file, handle) = fixture(|pile, collection, key| {
            pile.commit(
                collection,
                key,
                entity! {
                    metadata::tag: metadata::KIND_MULTI,
                    metadata::json_kind*: ["future-one", "future-two"],
                },
            )
            .unwrap();
        });
        let rows = evidence(&file, handle);
        assert_eq!(summary(&rows)["admitted_pairs"], 1);
        assert_eq!(summary(&rows)["readable_admitted_pairs"], 1);
        assert_eq!(summary(&rows)["decoded_admitted_data_tribles"], 3);
        assert_eq!(summary(&rows)["recursive_closure"], "unproved");
    }

    #[test]
    fn unreadable_descriptor_does_not_turn_admission_into_false() {
        let (file, _) = fixture(|_, _, _| {});
        let absent = CollectionHandle::new([93; 32]);
        let mut pile = PileFile::open(file.path()).unwrap();
        let key = SigningKey::from_bytes(&[31; 32]);
        let empty = pile.put::<SimpleArchive, _>(TribleSet::new()).unwrap();
        pile.insert(CollectionRecord::Commit(CollectionCommit::sign(
            &key,
            absent,
            Inline::new(empty.raw),
            empty,
        )))
        .unwrap();
        pile.close().unwrap();
        let rows = evidence(&file, absent);
        let commit = rows.iter().find(|row| row["kind"] == "commit").unwrap();
        assert_eq!(commit["writer_admission"]["status"], "unavailable");
        assert_eq!(summary(&rows)["valid_pairs_only_denied"], 0);
        assert_eq!(summary(&rows)["valid_pairs_with_unresolved_admission"], 1);
    }

    #[test]
    fn one_admitted_endorsement_is_enough_without_reclassifying_other_records() {
        let (file, handle) = fixture(|pile, collection, key| {
            let row = entity! { metadata::tag: metadata::KIND_MULTI };
            pile.commit(collection, &SigningKey::from_bytes(&[32; 32]), row.clone())
                .unwrap();
            pile.commit(collection, key, row).unwrap();
        });
        let rows = evidence(&file, handle);
        assert_eq!(summary(&rows)["known_commits"], 2);
        assert_eq!(summary(&rows)["raw_pairs"], 1);
        assert_eq!(summary(&rows)["admitted_pairs"], 1);
        assert_eq!(summary(&rows)["valid_pairs_only_denied"], 0);
    }

    #[test]
    fn replayed_physical_frames_do_not_inflate_record_identity_counts() {
        let (file, handle) = fixture(|pile, collection, key| {
            pile.commit(
                collection,
                key,
                entity! { metadata::tag: metadata::KIND_MULTI },
            )
            .unwrap();
        });
        let first = evidence(&file, handle);
        let bytes = std::fs::read(file.path()).unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(file.path())
            .unwrap()
            .write_all(&bytes)
            .unwrap();
        let replayed = evidence(&file, handle);
        assert_eq!(summary(&replayed)["known_commits"], 1);
        assert_eq!(summary(&first), summary(&replayed));
    }
}
