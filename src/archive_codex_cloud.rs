//! Projection of ChatGPT's Codex cloud tasks onto Archive's block DAG.
//!
//! A ChatGPT data export holds the tasks in `codex-000.json`, `codex-001.json`
//! and so on: JSON arrays of tasks, each an ordered list of turns that
//! alternate between the person (`user`) and the agent (`assistant`). A task
//! is one source unit and one chain of nodes.
//!
//! # The field table is the reader
//!
//! Every object is opened against a [`Shape`], which names every member the
//! export may hold and what becomes of it: [`Rule::Read`] (the projection must
//! take it), [`Rule::Is`] (a discriminator with one value), [`Rule::Null`] (a
//! member only ever seen as `null`: what a value would mean is not known) or
//! [`Rule::Drop`] with the reason it is left out. A member the shape does not
//! name, a missing one, a repeated one, a value where only `null` is known and
//! an unknown item type each fail the unit. A read member the projection did
//! not take fails it too ([`Opened::finish`]), so nothing is left out except by
//! name. Dropped members that held a value are counted by path in
//! [`ProjectionSummary::dropped`].
//!
//! # What becomes of a task
//!
//! - A user turn's message and inline review comments are perceived text. A
//!   comment's place in the diff (path, side, lines) follows its text as a
//!   perceived event holding that place as canonical JSON. Empty message text
//!   (a turn that holds only comments) is nothing.
//! - An IDE context is perceived text. A prior conversation handed to the task
//!   comes before the message, as its name says, each message in its own
//!   role's direction. A patch applied before the turn is a perceived file.
//! - An agent message is one utterance, which the export cuts into text runs at
//!   its citations. Its runs joined in order, with nothing between them, are
//!   one produced text. Where it cites, one produced event follows: a canonical
//!   JSON array of its citations in order, each `{"byte_offset": n,
//!   "citation": {..}}` with the citation whole and the place it stood in the
//!   joined text. Cutting the text at those offsets gives the runs back. A
//!   citation keeps its `terminal_chunk_id`: it is what the agent wrote in the
//!   citation (it sometimes names a file there), not an id the export gave.
//! - A proposed change (`pr`, `follow_up_diff`) is a produced event with the
//!   words and the place of the change (title, message, commit message,
//!   repository, base commit, line counts) and then the change itself, a
//!   produced file: the diff's text where the export holds it, otherwise an
//!   unresolved pointer to the file the export names and does not ship.
//!
//! The person's turn follows the turn listed before it; the agent's turn
//! follows the turn its `previous_turn_id` names, and where the task does not
//! hold that turn it follows nothing and is counted as a missing predecessor.
//! Ids thread the graph while projecting. The task id annotates the task's
//! leaves as `task/<id>`; no id enters an identity except inside the pointer
//! of a file that is not held.
//!
//! The export gives no time field. Task and turn ids carry the creation second
//! in their first eight hex digits, and that second is the timestamp
//! annotation of a turn's nodes. An id of another form, or seconds that do not
//! follow the turns' order, fail the unit. An agent's turn is created with its
//! prompt, so its nodes carry the second the turn began: the final message
//! came later, by an amount the export does not say, and none is invented. A
//! prior conversation's messages have no time.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anybytes::{Bytes, View};
use anyhow::{anyhow, bail, Context, Result};
use hifitime::Epoch;
use triblespace::core::import::scanner as sc;
use triblespace::core::inline::TryToInline;
use triblespace::prelude::inlineencodings::NsTAIInterval;
use triblespace::prelude::*;

use crate::archive_source::{
    self, ProjectedSource, ProjectionStats, SourceClaims, SourcePart, SourceRecord,
};
use crate::schemas::blockdag as schema;
use schema::content_fact::{direction, modality};

/// Accounting returned after every task reached `emit`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProjectionSummary {
    pub files_scanned: usize,
    pub tasks_seen: usize,
    pub turns_seen: usize,
    /// Message texts that were empty and so project to nothing.
    pub empty_texts: usize,
    /// Changes and patches whose text the export does not hold.
    pub unresolved_files: usize,
    /// Members left out that held a value, by path, with how many did.
    pub dropped: BTreeMap<&'static str, usize>,
    pub stats: ProjectionStats,
}

/// What becomes of one member.
#[derive(Clone, Copy, Debug)]
enum Rule {
    /// The projection takes it.
    Read,
    /// The member's only value.
    Is(&'static str),
    /// Only ever `null`; any other value fails the unit.
    Null,
    /// Left out; the path it is counted under, and why.
    Drop(&'static str, &'static str),
}

/// Every member an object of one kind holds.
struct Shape {
    name: &'static str,
    members: &'static [(&'static str, Rule)],
}

use Rule::{Drop, Is, Null, Read};

const UI: &str = "state of the task list, never in the agent's context";
const REVIEW: &str = "what the person did with the change afterwards";

const TASK: Shape = Shape {
    name: "task",
    members: &[
        ("archived", Drop("task.archived", UI)),
        ("id", Read),
        ("title", Drop("task.title", UI)),
        ("turns", Read),
    ],
};

const USER_TURN: Shape = Shape {
    name: "user turn",
    members: &[
        ("custom_instructions", Null),
        ("id", Read),
        ("input_items", Read),
        ("role", Is("user")),
    ],
};

const ASSISTANT_TURN: Shape = Shape {
    name: "assistant turn",
    members: &[
        (
            "branch",
            Drop("assistant_turn.branch", "name of the checkout's branch"),
        ),
        ("branch_name", Drop("assistant_turn.branch_name", REVIEW)),
        (
            "external_pull_request_id",
            Drop("assistant_turn.external_pull_request_id", REVIEW),
        ),
        ("id", Read),
        ("output_items", Read),
        ("previous_turn_id", Read),
        (
            "pull_request_status",
            Drop("assistant_turn.pull_request_status", REVIEW),
        ),
        ("role", Is("assistant")),
        (
            "turn_status",
            Drop("assistant_turn.turn_status", "state of the run"),
        ),
    ],
};

const INPUT_MESSAGE: Shape = Shape {
    name: "input message",
    members: &[
        ("content", Read),
        ("role", Is("user")),
        ("type", Is("message")),
    ],
};

const COMMENT: Shape = Shape {
    name: "comment",
    members: &[
        ("content", Read),
        ("position", Read),
        ("type", Is("comment")),
    ],
};

/// The whole place is kept, so its members are only checked to be these.
const POSITION: Shape = Shape {
    name: "comment position",
    members: &[
        ("line", Read),
        ("path", Read),
        ("side", Read),
        ("start_line", Read),
    ],
};

const IDE_CONTEXT: Shape = Shape {
    name: "IDE context",
    members: &[("context", Read), ("type", Is("ide_context"))],
};

const PRE_APPLY_PATCH: Shape = Shape {
    name: "pre-apply patch",
    members: &[("output_diff", Read), ("type", Is("pre_apply_patch"))],
};

const PRIOR_CONVERSATION: Shape = Shape {
    name: "prior conversation",
    members: &[
        ("conversation", Read),
        ("diff", Null),
        ("prior_task_id", Null),
        ("type", Is("prior_conversation")),
    ],
};

const PRIOR_MESSAGE: Shape = Shape {
    name: "prior message",
    members: &[("content", Read), ("role", Read), ("type", Is("message"))],
};

const OUTPUT_MESSAGE: Shape = Shape {
    name: "output message",
    members: &[
        ("content", Read),
        ("role", Is("assistant")),
        ("type", Is("message")),
    ],
};

const SNAPSHOT: Shape = Shape {
    name: "partial repository snapshot",
    members: &[
        (
            "files",
            Drop(
                "partial_repo_snapshot.files",
                "whole files the export adds for showing citations, not what the agent was shown",
            ),
        ),
        ("type", Is("partial_repo_snapshot")),
    ],
};

const PR: Shape = Shape {
    name: "pr",
    members: &[
        ("output_diff", Read),
        ("pr_message", Read),
        ("pr_title", Read),
        ("pre_apply_patch", Null),
        ("type", Is("pr")),
    ],
};

const FOLLOW_UP_DIFF: Shape = Shape {
    name: "follow-up diff",
    members: &[("output_diff", Read), ("type", Is("follow_up_diff"))],
};

const OUTPUT_DIFF: Shape = Shape {
    name: "output diff",
    members: &[
        ("additional_parent_commit_shas", Null),
        ("base_commit_sha", Read),
        ("commit_message", Read),
        ("diff", Read),
        ("external_storage_diff", Read),
        ("files_modified", Read),
        ("lines_added", Read),
        ("lines_removed", Read),
        ("preapply_patch_base_commit_sha", Null),
        ("repo_id", Read),
        ("type", Is("output_diff")),
    ],
};

const EXTERNAL_DIFF: Shape = Shape {
    name: "external diff",
    members: &[("file_id", Read), ("ttl", Null)],
};

const TEXT: Shape = Shape {
    name: "text",
    members: &[("content_type", Is("text")), ("text", Read)],
};

/// Citations are kept whole, so their members are only checked to be these.
const FILE_CITATION: Shape = Shape {
    name: "file citation",
    members: &[
        ("content_type", Is("repo_file_citation")),
        ("line_range_end", Read),
        ("line_range_start", Read),
        ("path", Read),
    ],
};

const TERMINAL_CITATION: Shape = Shape {
    name: "terminal citation",
    members: &[
        ("content_type", Is("terminal_chunk_citation")),
        ("line_range_end", Read),
        ("line_range_start", Read),
        ("terminal_chunk_id", Read),
    ],
};

/// One object opened against its shape.
struct Opened {
    shape: &'static Shape,
    unread: BTreeMap<&'static str, Bytes>,
}

impl Opened {
    /// The value of a member the shape reads.
    fn take(&mut self, name: &'static str) -> Result<Bytes> {
        self.unread
            .remove(name)
            .ok_or_else(|| anyhow!("{} has no unread member {name:?}", self.shape.name))
    }

    /// A string member.
    fn text(&mut self, name: &'static str) -> Result<View<str>> {
        let value = self.take(name)?;
        archive_source::json_str(&value)
            .ok_or_else(|| anyhow!("{}.{name} is not a string", self.shape.name))
    }

    /// An array member's elements.
    fn elements(&mut self, name: &'static str) -> Result<Vec<Bytes>> {
        let value = self.take(name)?;
        archive_source::json_elements(&value)
            .ok_or_else(|| anyhow!("{}.{name} is not an array", self.shape.name))
    }

    /// Every remaining member is kept by a caller that keeps the whole object.
    fn whole(&mut self) {
        self.unread.clear();
    }

    /// Fail if a member the shape reads was not taken.
    fn finish(self) -> Result<()> {
        if let Some(name) = self.unread.keys().next() {
            bail!("{}.{name} was read and not projected", self.shape.name);
        }
        Ok(())
    }
}

fn is_null(value: &Bytes) -> bool {
    value.as_ref() == b"null"
}

/// Whether a left-out value held anything.
fn holds(value: &Bytes) -> bool {
    !matches!(
        value.as_ref(),
        b"null" | b"\"\"" | b"[]" | b"{}" | b"false" | b"\"None\""
    )
}

/// A named string member of an object, before its shape is known.
fn discriminator(value: &Bytes, name: &str) -> Result<View<str>> {
    let [found] = archive_source::json_members(value, [name]);
    found
        .as_ref()
        .and_then(archive_source::json_str)
        .ok_or_else(|| anyhow!("an item has no string {name:?}"))
}

/// One projection call's accounting.
#[derive(Default)]
struct Reader {
    summary: ProjectionSummary,
}

impl Reader {
    fn open(&mut self, shape: &'static Shape, value: Bytes) -> Result<Opened> {
        if value.first().copied() != Some(b'{') {
            bail!("{} is not an object", shape.name);
        }
        let mut bytes = value;
        let members = sc::object(&mut bytes, Vec::new(), |mut members, key, member| {
            members.push((key, sc::take_value(member)?));
            Ok::<_, sc::ScanError>(members)
        })
        .map_err(|error| anyhow!(error))
        .with_context(|| format!("scan {}", shape.name))?;
        let mut seen = BTreeMap::new();
        for (key, member) in members {
            let Some((name, rule)) = shape
                .members
                .iter()
                .find(|(name, _)| name.as_bytes() == key.as_ref())
            else {
                bail!(
                    "{} has an unknown member {:?}",
                    shape.name,
                    String::from_utf8_lossy(key.as_ref())
                );
            };
            if seen.insert(*name, (*rule, member)).is_some() {
                bail!("{} repeats member {name:?}", shape.name);
            }
        }
        let mut unread = BTreeMap::new();
        for (name, _) in shape.members {
            let Some((rule, member)) = seen.remove(name) else {
                bail!("{} has no member {name:?}", shape.name);
            };
            match rule {
                Read => {
                    unread.insert(*name, member);
                }
                Is(expected) => {
                    let value = archive_source::json_str(&member);
                    if value.as_ref().map(AsRef::as_ref) != Some(expected) {
                        bail!("{}.{name} is not {expected:?}", shape.name);
                    }
                }
                Null => {
                    if !is_null(&member) {
                        bail!(
                            "{}.{name} holds a value, and only null is understood",
                            shape.name
                        );
                    }
                }
                Drop(path, _reason) => {
                    if holds(&member) {
                        *self.summary.dropped.entry(path).or_default() += 1;
                    }
                }
            }
        }
        Ok(Opened { shape, unread })
    }

    /// The texts of a content list that holds text items only.
    fn texts(&mut self, items: Vec<Bytes>) -> Result<Vec<View<str>>> {
        let mut texts = Vec::new();
        for item in items {
            let mut item = self.open(&TEXT, item)?;
            let text = item.text("text")?;
            item.finish()?;
            if text.as_ref().is_empty() {
                self.summary.empty_texts += 1;
            } else {
                texts.push(text);
            }
        }
        Ok(texts)
    }

    /// An agent message: its text runs joined, then its citations by place.
    fn message(&mut self, items: Vec<Bytes>, parts: &mut Vec<SourcePart>) -> Result<()> {
        let mut said = String::new();
        let mut citations = Vec::new();
        for item in items {
            let shape = match discriminator(&item, "content_type")?.as_ref() {
                "text" => {
                    for text in self.texts(vec![item])? {
                        said.push_str(text.as_ref());
                    }
                    continue;
                }
                "repo_file_citation" => &FILE_CITATION,
                "terminal_chunk_citation" => &TERMINAL_CITATION,
                other => bail!("an output message holds unknown content {other:?}"),
            };
            let mut citation = self.open(shape, item.clone())?;
            citation.whole();
            citation.finish()?;
            citations.push(object(&BTreeMap::from([
                ("byte_offset", said.len().to_string()),
                ("citation", canonical(item)?),
            ])));
        }
        if !said.is_empty() {
            let said = archive_source::owned_text(said);
            parts.push(SourcePart::text(modality::TEXT, direction::OUT, said));
        }
        if !citations.is_empty() {
            let citations = format!("[{}]", citations.join(","));
            parts.push(event(direction::OUT, citations));
        }
        Ok(())
    }

    /// A change's own words and place, and the change as a file.
    ///
    /// `words` are the members the item itself adds, as canonical JSON. A
    /// change the source gives no words or place for is a file alone.
    fn change(
        &mut self,
        value: Bytes,
        kind: &'static str,
        words: Vec<(&'static str, String)>,
        facing: Id,
        parts: &mut Vec<SourcePart>,
    ) -> Result<()> {
        let mut diff = self.open(&OUTPUT_DIFF, value)?;
        let mut members = BTreeMap::new();
        for (name, value) in words {
            members.insert(name, value);
        }
        for name in [
            "base_commit_sha",
            "commit_message",
            "files_modified",
            "lines_added",
            "lines_removed",
            "repo_id",
        ] {
            let value = diff.take(name)?;
            if !is_null(&value) {
                members.insert(name, canonical(value)?);
            }
        }
        let inline = diff.take("diff")?;
        let external = diff.take("external_storage_diff")?;
        diff.finish()?;

        if !members.is_empty() {
            members.insert(
                "type",
                canonical(Bytes::from_source(format!("\"{kind}\"")))?,
            );
            parts.push(event(facing, object(&members)));
        }
        let file = match (is_null(&inline), is_null(&external)) {
            (false, true) => SourcePart::text(
                modality::FILE,
                facing,
                archive_source::json_str(&inline)
                    .ok_or_else(|| anyhow!("output diff.diff is not a string"))?,
            ),
            (true, false) => {
                let mut external = self.open(&EXTERNAL_DIFF, external)?;
                let pointer = external.text("file_id")?;
                external.finish()?;
                self.summary.unresolved_files += 1;
                SourcePart::Pointer {
                    modality: modality::FILE,
                    direction: facing,
                    namespace: schema::source_projection::SOURCE_CODEX_CLOUD,
                    pointer,
                    media_type: None,
                    size: None,
                    resolved: None,
                    sanitized: false,
                    page_of: None,
                }
            }
            _ => bail!("an output diff holds not exactly one of its text and an external file"),
        };
        parts.push(file);
        Ok(())
    }

    /// A user turn: the conversation it was handed, then its own items.
    fn user_turn(
        &mut self,
        value: Bytes,
        task: &str,
        before: Option<View<str>>,
        records: &mut Vec<SourceRecord>,
    ) -> Result<(View<str>, u32)> {
        let mut turn = self.open(&USER_TURN, value)?;
        let id = turn.text("id")?;
        let (second, timestamp) = turn_time(task, id.as_ref(), "usertrn")?;
        let items = turn.elements("input_items")?;
        turn.finish()?;

        let mut previous = before;
        let mut parts = Vec::new();
        for item in items {
            match discriminator(&item, "type")?.as_ref() {
                "message" => {
                    let mut message = self.open(&INPUT_MESSAGE, item)?;
                    let content = message.elements("content")?;
                    message.finish()?;
                    for text in self.texts(content)? {
                        parts.push(SourcePart::text(modality::TEXT, direction::IN, text));
                    }
                }
                "comment" => {
                    let mut comment = self.open(&COMMENT, item)?;
                    let content = comment.elements("content")?;
                    let position = comment.take("position")?;
                    comment.finish()?;
                    for text in self.texts(content)? {
                        parts.push(SourcePart::text(modality::TEXT, direction::IN, text));
                    }
                    let mut place = self.open(&POSITION, position.clone())?;
                    place.whole();
                    place.finish()?;
                    parts.push(event(direction::IN, canonical(position)?));
                }
                "ide_context" => {
                    let mut context = self.open(&IDE_CONTEXT, item)?;
                    let text = context.text("context")?;
                    context.finish()?;
                    parts.push(SourcePart::text(modality::TEXT, direction::IN, text));
                }
                "pre_apply_patch" => {
                    let mut patch = self.open(&PRE_APPLY_PATCH, item)?;
                    let diff = patch.take("output_diff")?;
                    patch.finish()?;
                    self.change(
                        diff,
                        "pre_apply_patch",
                        Vec::new(),
                        direction::IN,
                        &mut parts,
                    )?;
                }
                "prior_conversation" => {
                    let mut prior = self.open(&PRIOR_CONVERSATION, item)?;
                    let messages = prior.elements("conversation")?;
                    prior.finish()?;
                    for message in messages {
                        let mut message = self.open(&PRIOR_MESSAGE, message)?;
                        let role = message.text("role")?;
                        let content = message.elements("content")?;
                        message.finish()?;
                        let facing = match role.as_ref() {
                            "user" => direction::IN,
                            "assistant" => direction::OUT,
                            other => bail!("a prior message has unknown role {other:?}"),
                        };
                        let parts = self
                            .texts(content)?
                            .into_iter()
                            .map(|text| SourcePart::text(modality::TEXT, facing, text))
                            .collect();
                        let locator = archive_source::owned_text(format!(
                            "{}/prior/{}",
                            id.as_ref(),
                            records.len()
                        ));
                        records.push(SourceRecord {
                            claims: SourceClaims {
                                raw_role: Some(role),
                                ..SourceClaims::default()
                            },
                            ..SourceRecord::new(
                                locator.clone(),
                                previous.into_iter().collect(),
                                parts,
                            )
                        });
                        previous = Some(locator);
                    }
                }
                other => bail!("a user turn holds an unknown item {other:?}"),
            }
        }
        records.push(record(
            id.clone(),
            previous.into_iter().collect(),
            timestamp,
            "user",
            parts,
        ));
        Ok((id, second))
    }

    /// An agent's turn: what it produced, in order.
    fn assistant_turn(
        &mut self,
        value: Bytes,
        task: &str,
        records: &mut Vec<SourceRecord>,
    ) -> Result<(View<str>, u32)> {
        let mut turn = self.open(&ASSISTANT_TURN, value)?;
        let id = turn.text("id")?;
        let (second, timestamp) = turn_time(task, id.as_ref(), "assttrn")?;
        let previous = turn.text("previous_turn_id")?;
        let items = turn.elements("output_items")?;
        turn.finish()?;

        let mut parts = Vec::new();
        for item in items {
            match discriminator(&item, "type")?.as_ref() {
                "message" => {
                    let mut message = self.open(&OUTPUT_MESSAGE, item)?;
                    let content = message.elements("content")?;
                    message.finish()?;
                    self.message(content, &mut parts)?;
                }
                "pr" => {
                    let mut pr = self.open(&PR, item)?;
                    let diff = pr.take("output_diff")?;
                    let words = vec![
                        ("pr_message", canonical(pr.take("pr_message")?)?),
                        ("pr_title", canonical(pr.take("pr_title")?)?),
                    ];
                    pr.finish()?;
                    self.change(diff, "pr", words, direction::OUT, &mut parts)?;
                }
                "follow_up_diff" => {
                    let mut follow_up = self.open(&FOLLOW_UP_DIFF, item)?;
                    let diff = follow_up.take("output_diff")?;
                    follow_up.finish()?;
                    self.change(
                        diff,
                        "follow_up_diff",
                        Vec::new(),
                        direction::OUT,
                        &mut parts,
                    )?;
                }
                "partial_repo_snapshot" => self.open(&SNAPSHOT, item)?.finish()?,
                other => bail!("an assistant turn holds an unknown item {other:?}"),
            }
        }
        records.push(record(
            id.clone(),
            vec![previous],
            timestamp,
            "assistant",
            parts,
        ));
        Ok((id, second))
    }

    /// One task's records and its id.
    fn task(&mut self, value: Bytes) -> Result<(View<str>, Vec<SourceRecord>)> {
        let mut task = self.open(&TASK, value)?;
        let id = task.text("id")?;
        let turns = task.elements("turns")?;
        task.finish()?;
        self.summary.tasks_seen += 1;

        let mut latest = id_second(id.as_ref(), "task_e_").context("task id")?;
        let mut records = Vec::new();
        let mut before: Option<View<str>> = None;
        let mut expected = "user";
        for (index, turn) in turns.into_iter().enumerate() {
            self.summary.turns_seen += 1;
            let role = discriminator(&turn, "role")?;
            if role.as_ref() != expected {
                bail!("turn {index} is not the {expected} turn the alternation expects");
            }
            let projected = match role.as_ref() {
                "user" => self.user_turn(turn, id.as_ref(), before.take(), &mut records),
                _ => self.assistant_turn(turn, id.as_ref(), &mut records),
            };
            let (turn, second) = projected.with_context(|| format!("turn {index}"))?;
            if second < latest {
                bail!("turn {index} carries an earlier second than what it follows");
            }
            latest = second;
            before = Some(turn);
            expected = if expected == "user" {
                "assistant"
            } else {
                "user"
            };
        }
        Ok((id, records))
    }

    /// Every task of one file, each with the key that orders it.
    fn file(
        &mut self,
        path: &Path,
        bytes: Bytes,
        projected: &mut Vec<(archive_source::ContentKey, Vec<ProjectedSource>)>,
    ) -> Result<()> {
        let document = archive_source::json_document(bytes).map_err(|error| anyhow!(error))?;
        let tasks = archive_source::json_elements(&document)
            .ok_or_else(|| anyhow!("a Codex cloud task file is not a JSON array"))?;
        self.summary.files_scanned += 1;
        for (index, task) in tasks.into_iter().enumerate() {
            let (id, records) = self.task(task).with_context(|| format!("task {index}"))?;
            let mut fragments = Vec::new();
            let stats = archive_source::project_records(
                schema::source_projection::SOURCE_CODEX_CLOUD,
                path,
                records,
                archive_source::UnitAnnotations {
                    source_locator: Some(format!("task/{}", id.as_ref())),
                    ..archive_source::UnitAnnotations::default()
                },
                |fragment| {
                    fragments.push(fragment);
                    Ok(())
                },
            )
            .with_context(|| format!("task {index}"))?;
            self.summary.stats.absorb(stats);
            if !fragments.is_empty() {
                projected.push((archive_source::content_key(&fragments), fragments));
            }
        }
        Ok(())
    }

    fn emit<F>(
        mut self,
        projected: Vec<(archive_source::ContentKey, Vec<ProjectedSource>)>,
        emit: F,
    ) -> Result<ProjectionSummary>
    where
        F: FnMut(ProjectedSource) -> Result<()>,
    {
        let repeated = archive_source::emit_in_content_order(projected, emit)?;
        self.summary.stats.projections_emitted -= repeated;
        self.summary.stats.content_parts -= repeated;
        Ok(self.summary)
    }
}

fn record(
    locator: View<str>,
    predecessors: Vec<View<str>>,
    timestamp: Inline<NsTAIInterval>,
    role: &'static str,
    parts: Vec<SourcePart>,
) -> SourceRecord {
    SourceRecord {
        block_timestamp: Some(timestamp),
        claims: SourceClaims {
            timestamp: Some(timestamp),
            raw_role: Some(archive_source::owned_text(role)),
            ..SourceClaims::default()
        },
        ..SourceRecord::new(locator, predecessors, parts)
    }
}

fn event(facing: Id, json: String) -> SourcePart {
    SourcePart::text(modality::EVENT, facing, archive_source::owned_text(json))
}

fn canonical(value: Bytes) -> Result<String> {
    archive_source::canonical_json(value).map_err(|error| anyhow!(error))
}

/// Canonical JSON of members whose names are plain and values canonical.
fn object(members: &BTreeMap<&'static str, String>) -> String {
    let members: Vec<_> = members
        .iter()
        .map(|(name, value)| format!("\"{name}\":{value}"))
        .collect();
    format!("{{{}}}", members.join(","))
}

/// The creation second an id of the form `<prefix><32 hex digits>` carries in
/// its first eight digits.
fn id_second(id: &str, prefix: &str) -> Result<u32> {
    id.strip_prefix(prefix)
        .filter(|rest| {
            rest.len() == 32
                && rest
                    .bytes()
                    .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        })
        .and_then(|rest| u32::from_str_radix(&rest[..8], 16).ok())
        .ok_or_else(|| anyhow!("not an id of the form {prefix}<32 hex digits>"))
}

/// The creation second of one turn and its instant, from `<task id>~<kind>_e_<32 hex digits>`.
fn turn_time(task: &str, id: &str, kind: &str) -> Result<(u32, Inline<NsTAIInterval>)> {
    let own = id
        .strip_prefix(task)
        .and_then(|rest| rest.strip_prefix('~'))
        .ok_or_else(|| anyhow!("a turn id does not extend its task's id"))?;
    let second = id_second(own, &format!("{kind}_e_")).context("turn id")?;
    let epoch = Epoch::from_unix_seconds(f64::from(second));
    let instant = (epoch, epoch)
        .try_to_inline()
        .map_err(|_| anyhow!("a turn id's time is not representable"))?;
    Ok((second, instant))
}

/// Project one task file, or a directory holding one or more of them.
///
/// Files are read in name order and tasks are emitted in the order of their
/// content, so a later export that moves a task to another file cuts the same
/// pieces. Nothing is emitted unless every file was read.
pub fn project_path<F>(path: &Path, emit: F) -> Result<ProjectionSummary>
where
    F: FnMut(ProjectedSource) -> Result<()>,
{
    let mut files = Vec::new();
    if path.is_dir() {
        for entry in fs::read_dir(path).with_context(|| format!("read {}", path.display()))? {
            let entry = entry.context("read Codex cloud export entry")?;
            let file = entry.path();
            if entry
                .file_type()
                .context("read Codex cloud entry type")?
                .is_file()
                && is_task_file(&file)
            {
                files.push(file);
            }
        }
        files.sort();
    } else {
        if !is_task_file(path) {
            bail!("{} is not a codex.json shard", path.display());
        }
        files.push(path.to_path_buf());
    }
    if files.is_empty() {
        bail!("no codex.json shards in {}", path.display());
    }
    let mut reader = Reader::default();
    let mut projected = Vec::new();
    for file in files {
        let bytes = archive_source::map_immutable_file(&file)?;
        reader
            .file(&file, bytes, &mut projected)
            .with_context(|| format!("project Codex cloud tasks {}", file.display()))?;
    }
    reader.emit(projected, emit)
}

/// Project one resident task file. `source_name` is never opened.
pub fn project_bytes<F>(source_name: &str, bytes: Bytes, emit: F) -> Result<ProjectionSummary>
where
    F: FnMut(ProjectedSource) -> Result<()>,
{
    let mut reader = Reader::default();
    let mut projected: Vec<(archive_source::ContentKey, Vec<ProjectedSource>)> = Vec::new();
    reader
        .file(Path::new(source_name), bytes, &mut projected)
        .with_context(|| format!("project Codex cloud tasks {source_name}"))?;
    reader.emit(projected, emit)
}

/// `codex.json`, or a shard of it: `codex-<digits>.json`.
fn is_task_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("codex"))
        .and_then(|rest| rest.strip_suffix(".json"))
        .is_some_and(|shard| {
            shard.is_empty()
                || shard.strip_prefix('-').is_some_and(|digits| {
                    !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
                })
        })
}

#[cfg(test)]
#[path = "archive_codex_cloud/tests.rs"]
mod tests;
