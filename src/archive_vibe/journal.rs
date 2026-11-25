use std::collections::{BTreeMap, BTreeSet};

use anybytes::{Bytes, View};
use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use memchr::memmem;
use sha2::{Digest, Sha256};
use triblespace::prelude::Id;

use crate::archive_source::{
    canonical_dynamic_json, canonical_json_string, json_document, json_elements, json_members,
    json_number, json_str, owned_text, JsonNumber, SourcePart, SourceRecord, ToolLink,
};
use crate::schemas::blockdag::content_fact::{direction as directions, modality as modalities};

use super::ProjectionSummary;

const DIGEST_FIELD: &[u8] = b",\"record_sha256\":\"";

fn field(value: &Bytes, name: &str) -> Option<Bytes> {
    let [value] = json_members(value, [name]);
    value
}

fn string(value: &Bytes, name: &str) -> Result<View<str>> {
    field(value, name)
        .as_ref()
        .and_then(json_str)
        .with_context(|| format!("missing Vibe string {name}"))
}

fn elements(value: &Bytes, name: &str) -> Result<Vec<Bytes>> {
    field(value, name)
        .as_ref()
        .and_then(json_elements)
        .with_context(|| format!("missing Vibe array {name}"))
}

fn integer(value: &Bytes, name: &str) -> Result<u64> {
    field(value, name)
        .as_ref()
        .and_then(json_number)
        .and_then(|number| match number {
            JsonNumber::PosInt(number) => Some(number),
            _ => None,
        })
        .with_context(|| format!("missing Vibe integer {name}"))
}

/// The writer hashes the canonical line with this top-level field omitted.
/// Checking the original bytes also avoids re-serializing private payloads.
fn digest(line: &Bytes) -> Option<String> {
    let start = memmem::rfind(line, DIGEST_FIELD)?;
    let end = start + DIGEST_FIELD.len() + 64;
    if line.get(end) != Some(&b'"') {
        return None;
    }
    let mut hash = Sha256::new();
    hash.update(&line[..start]);
    hash.update(line.get(end + 1..)?);
    Some(hex::encode(hash.finalize()))
}

#[derive(Clone)]
struct Message {
    wire: Bytes,
    seen: bool,
}

#[derive(Clone, Default)]
struct Input {
    messages: Vec<Message>,
    catalog: Option<View<str>>,
}

#[derive(Default)]
pub(super) struct Scanner {
    records: Vec<SourceRecord>,
    revision: Option<u64>,
    input: Input,
    catalog_revision: Option<u64>,
    delivered_catalog: Option<View<str>>,
    /// One cache observation at the time of each unresolved agent model call.
    /// The provider can fail after the intent, or the run can stop there.
    pending: BTreeMap<String, Input>,
    failed: BTreeMap<String, Input>,
    streamed: BTreeSet<String>,
    streamed_entries: BTreeSet<String>,
    /// A completion is already an act, whether the Core subsequently commits
    /// it back to the model cache or discards it.
    last_reply: Option<[u8; 32]>,
}

impl Scanner {
    pub(super) fn scan_segment(
        &mut self,
        first: u64,
        bytes: Bytes,
        summary: &mut ProjectionSummary,
    ) -> Result<u64> {
        let mut offset = 0;
        let mut sequence = first;
        let mut previous = None;
        for newline in memchr::memchr_iter(b'\n', &bytes) {
            let line = bytes.slice(offset..newline);
            let current = self
                .scan_record(line, sequence, previous.as_deref(), summary)
                .with_context(|| format!("Vibe journal sequence {sequence}"))?;
            previous = Some(current);
            sequence = sequence.checked_add(1).context("Vibe sequence overflow")?;
            summary.journal_records += 1;
            offset = newline + 1;
        }
        // Newline is the record boundary even in a retained, formerly active
        // segment. The next segment's first sequence still has to join it.
        summary.trailing_bytes_held += bytes.len() - offset;
        Ok(sequence)
    }

    fn scan_record(
        &mut self,
        line: Bytes,
        sequence: u64,
        previous: Option<&str>,
        summary: &mut ProjectionSummary,
    ) -> Result<String> {
        let document = json_document(line).context("invalid Vibe journal record JSON")?;
        let [payload, record_type, prior, hash] = json_members(
            &document,
            ["payload", "type", "previous_record_sha256", "record_sha256"],
        );
        let hash = hash
            .as_ref()
            .and_then(json_str)
            .context("missing Vibe record digest")?;
        let prior = prior.context("missing Vibe previous-record digest")?;
        let previous_matches = match previous {
            Some(previous) => json_str(&prior).as_deref() == Some(previous),
            None => prior.as_ref() == b"null",
        };
        if integer(&document, "recovery_journal_record_version")? != 1
            || integer(&document, "sequence")? != sequence
            || digest(&document).as_deref() != Some(&*hash)
            || !previous_matches
        {
            bail!("Vibe record version, sequence or digest chain mismatch");
        }
        let payload = payload.context("missing Vibe record payload")?;
        match record_type.as_ref().and_then(json_str).as_deref() {
            Some("action_intent") => self.intent(&payload)?,
            Some("action_result") => self.result(&payload, summary)?,
            Some("projection_delta") => self.projection_delta(&payload, summary)?,
            Some(_) => {}
            None => bail!("missing Vibe record type"),
        }
        Ok(hash.to_string())
    }

    fn intent(&mut self, payload: &Bytes) -> Result<()> {
        if &*string(payload, "kind")? != "completion" {
            return Ok(());
        }
        let request = field(payload, "request").context("missing Vibe completion request")?;
        if &*string(&request, "type")? != "llm_call" {
            bail!("unexpected Vibe completion request type");
        }
        let id = string(payload, "action_id")?;
        if string(&request, "action_id")? != id {
            bail!("Vibe completion request action id mismatch");
        }
        let input = field(&request, "model_input").context("missing Vibe model input update")?;
        self.update_messages(&field(&input, "messages").context("missing Vibe message update")?)?;
        self.update_catalog(
            &field(&input, "tool_catalog").context("missing Vibe catalog update")?,
        )?;
        if &*string(&request, "purpose")? == "agent"
            && self
                .pending
                .insert(id.to_string(), self.input.clone())
                .is_some()
        {
            bail!("duplicate pending Vibe completion");
        }
        Ok(())
    }

    fn update_messages(&mut self, input: &Bytes) -> Result<()> {
        let revision = integer(input, "revision")?;
        let mut new: Vec<_> = elements(input, "messages")?
            .into_iter()
            .map(|wire| Message { wire, seen: false })
            .collect();
        match &*string(input, "type")? {
            "append" => {
                let base = integer(input, "base_revision")?;
                if self.revision != Some(base) || revision < base {
                    bail!("Vibe model input append did not name the held revision");
                }
            }
            "replace" => {
                // Vibe may replace the cache after a resume or compaction.
                // Keep already-projected context where the cache copies it
                // verbatim, including a retained tail after a new summary.
                let prefix = preserve_seen(self.input.messages.iter(), new.iter_mut());
                preserve_seen(
                    self.input.messages[prefix..].iter().rev(),
                    new[prefix..].iter_mut().rev(),
                );
                self.input.messages.clear();
            }
            _ => bail!("unknown Vibe model input update"),
        }
        if let Some(reply) = self.last_reply {
            for message in &mut new {
                if !message.seen
                    && &*string(&message.wire, "role")? == "assistant"
                    && assistant_signature(&elements(&message.wire, "content")?)? == reply
                {
                    message.seen = true;
                    self.last_reply = None;
                    break;
                }
            }
        }
        self.input.messages.extend(new);
        self.revision = Some(revision);
        Ok(())
    }

    fn update_catalog(&mut self, input: &Bytes) -> Result<()> {
        let revision = integer(input, "revision")?;
        match &*string(input, "type")? {
            "keep" if self.catalog_revision.unwrap_or(0) == revision => {}
            "replace" => {
                let tools = field(input, "tools").context("missing Vibe tool definitions")?;
                if !tools.starts_with(b"[") {
                    bail!("Vibe tool catalog is not an array");
                }
                let tools =
                    canonical_dynamic_json(tools).context("invalid Vibe tool definitions")?;
                self.input.catalog = Some(owned_text(format!(r#"{{"tool_catalog":{tools}}}"#)));
            }
            "keep" => bail!("Vibe tool catalog keep did not name the held revision"),
            _ => bail!("unknown Vibe tool catalog update"),
        }
        self.catalog_revision = Some(revision);
        Ok(())
    }

    fn result(&mut self, payload: &Bytes, summary: &mut ProjectionSummary) -> Result<()> {
        let id = string(payload, "action_id")?;
        let Some(mut input) = self.pending.remove(&*id) else {
            return Ok(());
        };
        let result = field(payload, "result").context("missing Vibe completion result")?;
        if string(&result, "action_id")? != id {
            bail!("Vibe completion result action id mismatch");
        }
        match &*string(&result, "type")? {
            "completion_failed" => {
                self.failed.insert(id.to_string(), input);
                summary.failed_calls += 1;
            }
            "completion_succeeded" => {
                self.deliver(&mut input, summary)?;
                let reply = field(&result, "result").context("missing Vibe completion reply")?;
                let parts = elements(&reply, "parts")?;
                self.last_reply = Some(assistant_signature(&parts)?);
                self.record(
                    owned_text("assistant"),
                    &parts,
                    directions::OUT,
                    None,
                    summary,
                )?;
                summary.completed_calls += 1;
            }
            _ => bail!("unknown Vibe agent completion result"),
        }
        Ok(())
    }

    fn deliver(&mut self, input: &mut Input, summary: &mut ProjectionSummary) -> Result<()> {
        for message in input.messages.iter_mut().filter(|message| !message.seen) {
            message.seen = true;
            let role = string(&message.wire, "role")?;
            if &*role == "assistant" {
                // A reused or rewritten assistant message is not a new
                // completion. Keep this loss visible in the receipt.
                summary.unmatched_assistant_inputs += 1;
                continue;
            }
            let tool = if &*role == "tool" {
                Some(string(&message.wire, "tool_call_id")?)
            } else {
                None
            };
            self.record(
                role,
                &elements(&message.wire, "content")?,
                directions::IN,
                tool.as_ref(),
                summary,
            )?;
        }
        preserve_seen(input.messages.iter(), self.input.messages.iter_mut());
        if self.delivered_catalog != input.catalog {
            if let Some(catalog) = &input.catalog {
                let mut record = self.next_record(owned_text("tool_catalog"));
                record.parts.push(SourcePart::text(
                    modalities::EVENT,
                    directions::IN,
                    catalog.clone(),
                ));
                self.records.push(record);
                summary.catalog_replacements += 1;
            }
            self.delivered_catalog = input.catalog.clone();
        }
        Ok(())
    }

    /// A provisional stream is normally memory-only. When the runtime settles
    /// nonempty discarded content, it journals that actual provider output.
    /// Use it only for an action without a successful authoritative reply.
    fn projection_delta(&mut self, payload: &Bytes, summary: &mut ProjectionSummary) -> Result<()> {
        for op in elements(payload, "delta")? {
            let entries = match &*string(&op, "op")? {
                "append_entry" | "replace_entry" => {
                    vec![field(&op, "entry").context("Vibe delta is missing its entry")?]
                }
                "set_history_entries" => elements(&op, "entries")?,
                _ => continue,
            };
            for entry in entries {
                let Some((id, action, parts)) = discarded_stream(&entry)? else {
                    continue;
                };
                if self.streamed_entries.contains(&*id) {
                    continue;
                }
                if !self.streamed.contains(&action) {
                    let Some(mut input) = self
                        .failed
                        .remove(&action)
                        .or_else(|| self.pending.get(&action).cloned())
                    else {
                        continue;
                    };
                    self.deliver(&mut input, summary)?;
                    if let Some(pending) = self.pending.get_mut(&action) {
                        *pending = input;
                    }
                    self.streamed.insert(action);
                    summary.streamed_calls += 1;
                }
                self.streamed_entries.insert(id.to_string());
                let mut record = self.next_record(owned_text("assistant"));
                record.claims.raw_outcome = Some(owned_text("discarded"));
                record.parts = parts;
                self.records.push(record);
            }
        }
        Ok(())
    }

    fn next_record(&self, role: View<str>) -> SourceRecord {
        let key = owned_text(self.records.len().to_string());
        let previous = self.records.last().map(|record| record.locator.clone());
        let mut record = SourceRecord::new(key, previous.into_iter().collect(), Vec::new());
        record.claims.raw_role = Some(role);
        record
    }

    fn record(
        &mut self,
        role: View<str>,
        parts: &[Bytes],
        direction: Id,
        tool: Option<&View<str>>,
        summary: &mut ProjectionSummary,
    ) -> Result<()> {
        let mut record = self.next_record(role);
        for part in parts {
            append_part(&mut record, part, direction, tool, summary)?;
        }
        self.records.push(record);
        Ok(())
    }

    pub(super) fn finish(self) -> (Vec<SourceRecord>, usize) {
        let unresolved = self
            .pending
            .keys()
            .filter(|id| !self.streamed.contains(*id))
            .count();
        (self.records, unresolved)
    }
}

fn discarded_stream(entry: &Bytes) -> Result<Option<(View<str>, String, Vec<SourcePart>)>> {
    let [status, outcome, id] = json_members(entry, ["generationStatus", "outcome", "id"]);
    if status.as_ref().and_then(json_str).as_deref() != Some("completed")
        || outcome
            .as_ref()
            .and_then(|outcome| field(outcome, "type"))
            .as_ref()
            .and_then(json_str)
            .as_deref()
            != Some("discarded")
    {
        return Ok(None);
    }
    let Some(id) = id.as_ref().and_then(json_str) else {
        return Ok(None);
    };
    let (action, modality, texts) = match &*string(entry, "type")? {
        "message" if id.starts_with("assistant-") => (
            id.trim_start_matches("assistant-"),
            modalities::TEXT,
            elements(entry, "content")?
                .iter()
                .filter_map(|part| field(part, "text").as_ref().and_then(json_str))
                .collect::<Vec<_>>(),
        ),
        "reasoning" if id.starts_with("reasoning-") && id.ends_with("-0") => (
            &id["reasoning-".len()..id.len() - 2],
            modalities::THINKING,
            vec![string(entry, "text")?],
        ),
        _ => return Ok(None),
    };
    let parts: Vec<_> = texts
        .into_iter()
        .filter(|text| !text.is_empty())
        .map(|text| SourcePart::text(modality, directions::OUT, text))
        .collect();
    let action = action.to_owned();
    Ok((!parts.is_empty()).then_some((id, action, parts)))
}

fn preserve_seen<'a>(
    old: impl Iterator<Item = &'a Message>,
    new: impl Iterator<Item = &'a mut Message>,
) -> usize {
    let mut kept = 0;
    for (old, new) in old.zip(new) {
        if old.wire != new.wire {
            break;
        }
        new.seen = old.seen;
        kept += 1;
    }
    kept
}

fn tool_call(part: &Bytes) -> Result<(View<str>, String)> {
    let id = string(part, "id")?;
    let name = canonical_json_string(&string(part, "name")?);
    // On the next input Vibe wraps those same arguments as {raw, value, type}.
    // The raw form also carries malformed JSON exactly as the model wrote it.
    let raw = match field(part, "arguments") {
        Some(arguments) => string(&arguments, "raw")?,
        None => string(part, "arguments_json")?,
    };
    let arguments = canonical_dynamic_json(Bytes::from_source(raw.as_bytes().to_vec()))
        .unwrap_or_else(|_| canonical_json_string(&raw));
    Ok((id, format!(r#"{{"arguments":{arguments},"name":{name}}}"#)))
}

fn assistant_signature(parts: &[Bytes]) -> Result<[u8; 32]> {
    let mut hash = blake3::Hasher::new();
    for part in parts {
        if &*string(part, "type")? == "tool_call" {
            let (id, value) = tool_call(part)?;
            hash.update(id.as_bytes());
            hash.update(&[0]);
            hash.update(value.as_bytes());
        } else {
            hash.update(part);
        }
        hash.update(&[0]);
    }
    Ok(*hash.finalize().as_bytes())
}

fn append_part(
    record: &mut SourceRecord,
    part: &Bytes,
    direction: Id,
    tool: Option<&View<str>>,
    summary: &mut ProjectionSummary,
) -> Result<()> {
    let start = record.parts.len();
    let kind = string(part, "type")?;
    match &*kind {
        "text" => {
            let modality = if tool.is_some() {
                modalities::TOOL_RESULT
            } else {
                modalities::TEXT
            };
            record
                .parts
                .push(SourcePart::text(modality, direction, string(part, "text")?));
        }
        "tool_call" if direction == directions::OUT => {
            let (id, value) = tool_call(part)?;
            record.links.push((start, ToolLink::Call(id)));
            record.parts.push(SourcePart::text(
                modalities::TOOL_CALL,
                direction,
                owned_text(value),
            ));
        }
        "reasoning" => {
            for content in elements(part, "content")? {
                match &*string(&content, "type")? {
                    "text" | "summary" => record.parts.push(SourcePart::text(
                        modalities::THINKING,
                        direction,
                        string(&content, "text")?,
                    )),
                    "redacted" => {
                        let data = string(&content, "data")?;
                        let (bytes, media_type) = crate::archive_source::sealed_bytes(
                            &data,
                            "application/vnd.mistral.vibe.redacted-reasoning",
                            crate::archive_source::SealedAlphabet::Standard,
                        );
                        record.parts.push(SourcePart::Blob {
                            modality: modalities::THINKING,
                            direction,
                            bytes: Bytes::from_source(bytes),
                            media_type: owned_text(media_type),
                        });
                    }
                    _ => summary.unsupported_parts += 1,
                }
            }
        }
        "image" | "audio" => {
            let data = string(part, "data")?;
            let media_type = string(part, "mime_type").or_else(|_| string(part, "mimeType"))?;
            let modality = if &*kind == "image" {
                modalities::IMAGE
            } else {
                modalities::AUDIO
            };
            record.parts.push(SourcePart::Blob {
                modality,
                direction,
                bytes: Bytes::from_source(
                    STANDARD
                        .decode(data.as_bytes())
                        .context("invalid Vibe media encoding")?,
                ),
                media_type,
            });
        }
        _ => summary.unsupported_parts += 1,
    }
    if let Some(tool) = tool {
        record.links.extend(
            (start..record.parts.len()).map(|index| (index, ToolLink::Result(tool.clone()))),
        );
    }
    Ok(())
}
