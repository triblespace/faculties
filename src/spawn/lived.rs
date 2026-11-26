//! What one run of a zooid lived: the opening input its launch prepared, and
//! the turns its own conversation holds in the Archive, in causal order.
//!
//! Only recorded relations join a run to its history, never time or text
//! that happens to sit nearby:
//!
//! - A run that records one conversation (`source_projection::source_locator`
//!   on the run: a Claude session, chosen per run at launch, or the agy
//!   conversation its settlement read) has as its history every archived
//!   node from that conversation's root to its newest held leaf, when no
//!   other run names the same conversation and it continues no earlier life
//!   ([`crate::schemas::spawn::spawn::archive_start`]).
//! - A conversation several runs name is shared. Spawn bounds each run's part
//!   only by bytes of the provider's transcript, and nothing in the Archive
//!   says which nodes those bytes became, so the history says so instead of
//!   handing every run the whole conversation ([`History::Shared`]). The
//!   whole conversation stays readable, labelled as the conversation.
//! - A local (Drive) run that resumed from an archived node
//!   ([`crate::schemas::spawn::spawn::resumes`]) lived the Drive context of
//!   its zooid's name that continues exactly that node
//!   (`context::continues`), and its history is the nodes observed in that
//!   context. A fresh local run records no such link.
//!
//! The prepared opening input is the run's `prompt`: what the launch fed the
//! agent, kept whole. It is no evidence that every byte reached the model;
//! where the archived conversation's first input is a different text, both
//! stay and neither replaces the other ([`opening_agrees`]).
//!
//! Every function here is a query over facts the caller holds and a blob
//! reader; nothing is written, and nothing outside those facts is fetched.

use std::collections::BTreeSet;

use anybytes::View;
use anyhow::Result;
use triblespace::core::metadata;
use triblespace::core::repo::BlobStoreGet;
use triblespace::prelude::*;

use super::TextHandle;
use crate::schemas::archive_v3::{context, fact as v3_fact, occurrence};
use crate::schemas::blockdag as archive_schema;
use crate::schemas::spawn::{spawn as attrs, KIND_RUN};
use crate::storage::FactArchive;

/// What one run lived, as far as the facts say.
#[derive(Clone, Debug)]
pub struct Life {
    pub run: Id,
    /// The prepared opening input(s) the launch recorded, each kept.
    pub opening: Vec<TextHandle>,
    pub history: History,
}

/// Where a run's turns are, or why they cannot be shown as its own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum History {
    /// The run's own archived nodes, roots first, and the recorded relation
    /// that makes them its own.
    Own { nodes: Vec<Id>, basis: String },
    /// A conversation several lives share, with no recorded per-run node
    /// boundary. `runs` are the runs that name it, this one included.
    Shared {
        locator: String,
        runs: Vec<Id>,
        why: String,
    },
    /// The run's history is known to be empty.
    Empty(String),
    /// The run names a history this Archive does not hold (yet).
    NotArchived(String),
    /// Nothing recorded links this run to archived history.
    Unlinked(String),
    /// The Archive cannot be read here.
    Unavailable(String),
}

/// Who produced a node, from the direction of the facts it holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// The zooid perceived it (`in`).
    Received,
    /// The zooid produced it (`out`).
    Produced,
    /// It holds facts of both directions, or of none.
    Unknown,
}

/// One part of a node, as the viewer shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Body {
    /// The exact text.
    Text(String),
    /// The text's blob is not resident in this snapshot.
    Missing(String),
    /// Bytes the viewer does not render as text (an image, sealed
    /// reasoning), or a pointer to bytes the source did not hold.
    Other(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Part {
    /// `text`, `thinking`, `tool-call`, `tool-result`, ...
    pub kind: String,
    pub body: Body,
}

/// One archived node, read for display.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Turn {
    pub node: Id,
    pub direction: Direction,
    /// The role spellings its source records claim (`user`, `assistant`).
    pub roles: Vec<String>,
    pub parts: Vec<Part>,
    /// The call node(s) this node answers (`block::responds_to`).
    pub answers: Vec<Id>,
    /// The node(s) that answer this one as their call.
    pub answered_by: Vec<Id>,
    /// A recorded classifier stop followed it: its content is not read.
    pub withheld: bool,
    /// Its first `in` text, by handle, for comparing with the opening.
    pub first_input: Option<TextHandle>,
}

fn read_text<R: BlobStoreGet + ?Sized>(reader: &R, handle: TextHandle) -> Option<String> {
    reader
        .get::<View<str>, _>(handle)
        .ok()
        .map(|text| text.to_string())
}

fn counts(values: impl Iterator<Item = Inline<inlineencodings::U256BE>>) -> Vec<u64> {
    values
        .filter_map(|value| value.try_from_inline::<u64>().ok())
        .collect()
}

/// What `run` lived: its prepared opening input and where its turns are.
/// `archive` is this workspace's routed Archive, or why it is not here.
pub fn life<R: BlobStoreGet + ?Sized>(
    spawn: &FactArchive,
    reader: &R,
    archive: Result<&FactArchive, &str>,
    run: Id,
) -> Result<Life> {
    let opening: Vec<TextHandle> = find!(
        handle: TextHandle,
        pattern!(spawn, [{ run @ attrs::prompt: ?handle }])
    )
    .collect();
    let history = history(spawn, reader, archive, run)?;
    Ok(Life {
        run,
        opening,
        history,
    })
}

fn history<R: BlobStoreGet + ?Sized>(
    spawn: &FactArchive,
    reader: &R,
    archive: Result<&FactArchive, &str>,
    run: Id,
) -> Result<History> {
    let ended = exists!(pattern!(spawn, [{ run @ metadata::finished_at: _?at }]));
    let imported = counts(find!(
        value: Inline<inlineencodings::U256BE>,
        pattern!(spawn, [{ run @ attrs::archive_imported: ?value }])
    ));
    let locators: Vec<TextHandle> = find!(
        handle: TextHandle,
        pattern!(spawn, [{ run @ archive_schema::source_projection::source_locator: ?handle }])
    )
    .collect();
    let resumes: Vec<Id> =
        find!(node: Id, pattern!(spawn, [{ run @ attrs::resumes: ?node }])).collect();

    match locators.as_slice() {
        [] if !resumes.is_empty() => drive_history(spawn, archive, run, &resumes),
        [] => {
            if imported.contains(&0) {
                return Ok(History::Empty(
                    "Its settlement found it left no conversation to archive.".to_owned(),
                ));
            }
            let fresh_local = find!(
                handle: TextHandle,
                pattern!(spawn, [{ run @ attrs::provider: ?handle }])
            )
            .filter_map(|handle| read_text(reader, handle))
            .any(|provider| provider == "local");
            Ok(History::Unlinked(if fresh_local {
                "A fresh local run records no link to the Drive context it lived, so its turns \
                 cannot be told apart from other contexts of its name."
                    .to_owned()
            } else if ended {
                "This run records no conversation the Archive could name.".to_owned()
            } else {
                "This run records no conversation yet; an agy run names its conversation when \
                 it settles."
                    .to_owned()
            }))
        }
        [handle] => {
            let Some(locator) = read_text(reader, *handle) else {
                return Ok(History::Unavailable(
                    "The name of this run's conversation is not resident here.".to_owned(),
                ));
            };
            let handle = *handle;
            let mut runs: Vec<(Option<IntervalValueKey>, Id)> = find!(
                other: Id,
                pattern!(spawn, [{ ?other @
                    metadata::tag: KIND_RUN,
                    archive_schema::source_projection::source_locator: handle,
                }])
            )
            .chain(std::iter::once(run))
            .collect::<BTreeSet<Id>>()
            .into_iter()
            .map(|other| (started(spawn, other), other))
            .collect();
            runs.sort();
            let runs: Vec<Id> = runs.into_iter().map(|(_, id)| id).collect();
            let start = counts(find!(
                value: Inline<inlineencodings::U256BE>,
                pattern!(spawn, [{ run @ attrs::archive_start: ?value }])
            ));
            let end = counts(find!(
                value: Inline<inlineencodings::U256BE>,
                pattern!(spawn, [{ run @ attrs::archive_end: ?value }])
            ));
            if runs.len() > 1 || start.iter().any(|bytes| *bytes > 0) {
                let part = match (start.as_slice(), end.as_slice()) {
                    ([start], [end]) => format!("bytes {start} to {end}"),
                    ([start], _) => format!("the bytes after {start}"),
                    (_, [end]) => format!("bytes 0 to {end}"),
                    _ => "a part not bounded in bytes".to_owned(),
                };
                let others = runs.len() - 1;
                return Ok(History::Shared {
                    why: format!(
                        "{} name this conversation. Spawn bounds this run's part only in bytes \
                         of the provider's transcript ({part}); nothing recorded says which \
                         archived nodes those bytes became, so this run's own turns cannot be \
                         told apart from {}.",
                        if others == 0 {
                            "Earlier lives".to_owned()
                        } else {
                            format!("{} runs", runs.len())
                        },
                        if others == 0 {
                            "the earlier lives'"
                        } else {
                            "the other lives'"
                        },
                    ),
                    locator,
                    runs,
                });
            }
            let archive = match archive {
                Ok(archive) => archive,
                Err(reason) => return Ok(History::Unavailable(format!("Archive {reason}."))),
            };
            match crate::archive::conversation_nodes(archive, &locator)? {
                Some((leaf, nodes)) => Ok(History::Own {
                    nodes,
                    basis: format!(
                        "Its own conversation {locator}, which no other run here names: every \
                         archived node from its root to its newest held leaf {leaf:x}."
                    ),
                }),
                None if imported.contains(&0) => Ok(History::Empty(format!(
                    "Its settlement found {locator} held no complete line to archive."
                ))),
                None => Ok(History::NotArchived(match imported.iter().max() {
                    Some(bytes) => format!(
                        "Its settlement archived {bytes} bytes of {locator}, but this \
                         workspace's Archive holds no node of it."
                    ),
                    None if !ended => format!(
                        "{locator} is not archived yet: the run has not ended, and its \
                         settlement archives it."
                    ),
                    None => format!(
                        "{locator} is not in this workspace's Archive, and no import of it \
                         is recorded on the run."
                    ),
                })),
            }
        }
        several => Ok(History::Unlinked(format!(
            "This run records {} conversations, so none of them is its one history.",
            several.len()
        ))),
    }
}

/// A run's start, for ordering the runs of a conversation.
type IntervalValueKey = i128;

fn started(spawn: &FactArchive, run: Id) -> Option<IntervalValueKey> {
    find!(
        at: super::IntervalValue,
        pattern!(spawn, [{ run @ metadata::started_at: ?at }])
    )
    .filter_map(|at| {
        at.try_from_inline::<(hifitime::Epoch, hifitime::Epoch)>()
            .ok()
    })
    .map(|(start, _)| start.to_tai_duration().total_nanoseconds())
    .min()
}

/// A local run's history: the Drive context of its zooid's name that
/// continues the node it resumed from.
fn drive_history(
    spawn: &FactArchive,
    archive: Result<&FactArchive, &str>,
    run: Id,
    resumes: &[Id],
) -> Result<History> {
    let archive = match archive {
        Ok(archive) => archive,
        Err(reason) => return Ok(History::Unavailable(format!("Archive {reason}."))),
    };
    let names: Vec<TextHandle> = find!(
        name: TextHandle,
        pattern!(spawn, [
            { run @ attrs::of: _?zooid },
            { _?zooid @ attrs::name: ?name },
        ])
    )
    .collect();
    let mut contexts = BTreeSet::new();
    for &node in resumes {
        for &name in &names {
            contexts.extend(find!(
                ctx: Id,
                pattern!(archive, [{ ?ctx @ context::continues: node, context::zooid: name }])
            ));
        }
    }
    let resumed = resumes
        .iter()
        .map(|node| format!("{node:x}"))
        .collect::<Vec<_>>()
        .join(", ");
    match contexts.len() {
        0 => Ok(History::NotArchived(format!(
            "No archived Drive context of this zooid continues {resumed}, the node it resumed \
             from."
        ))),
        1 => {
            let ctx = *contexts.first().expect("one context");
            let nodes: BTreeSet<Id> = find!(
                node: Id,
                pattern!(archive, [
                    { _?occurrence @ occurrence::of: _?witness, context::in_context: ctx },
                    { _?witness @ archive_schema::source_projection::projects_to: ?node },
                ])
            )
            .collect();
            if nodes.is_empty() {
                return Ok(History::Empty(format!(
                    "Its Drive context {ctx:x} holds no observed node."
                )));
            }
            Ok(History::Own {
                nodes: crate::archive::order_nodes(archive, &nodes)?,
                basis: format!(
                    "The nodes observed in its Drive context {ctx:x}, which continues {resumed}, \
                     the node this run resumed from."
                ),
            })
        }
        _ => Ok(History::Unlinked(format!(
            "Drive contexts {} all continue {resumed} under this zooid's name; nothing records \
             which one this run lived.",
            contexts
                .iter()
                .map(|ctx| format!("{ctx:x}"))
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// Every node of the conversation `locator` names, roots first: the whole
/// shared conversation, for a reader who asks for it as that.
pub fn conversation(archive: &FactArchive, locator: &str) -> Result<Option<Vec<Id>>> {
    Ok(crate::archive::conversation_nodes(archive, locator)?.map(|(_, nodes)| nodes))
}

fn label(specs: &[(Id, &str)], id: Id) -> String {
    specs
        .iter()
        .find(|(term, _)| *term == id)
        .map(|(_, name)| (*name).to_owned())
        .unwrap_or_else(|| format!("{id:x}"))
}

/// The nodes `nodes`, read for display: their parts with exact text, the
/// roles their records claim, the calls they answer. `stopped` are the
/// nodes a recorded classifier stop followed
/// (`crate::archive::classifier_stopped`), whose content is not read.
pub fn turns<R: BlobStoreGet + ?Sized>(
    facts: &FactArchive,
    reader: &R,
    nodes: &[Id],
    stopped: &BTreeSet<Id>,
) -> Vec<Turn> {
    use archive_schema::content_fact::direction::{IN, OUT};
    nodes
        .iter()
        .map(|&node| {
            let mut roles: Vec<String> = find!(
                role: TextHandle,
                pattern!(facts, [{ _?receipt @
                    archive_schema::source_projection::projects_to: node,
                    archive_schema::source_projection::raw_role: ?role,
                }])
            )
            .chain(find!(
                role: TextHandle,
                pattern!(facts, [
                    { _?witness @ archive_schema::source_projection::projects_to: node },
                    { _?occurrence @ occurrence::of: _?witness,
                      archive_schema::source_projection::raw_role: ?role },
                ])
            ))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter_map(|role| read_text(reader, role))
            .collect();
            roles.dedup();
            let answers: Vec<Id> = find!(
                call: Id,
                pattern!(facts, [{ node @ archive_schema::block::responds_to: ?call }])
            )
            .collect();
            let answered_by: Vec<Id> = find!(
                result: Id,
                pattern!(facts, [{ ?result @ archive_schema::block::responds_to: node }])
            )
            .collect();
            let held: Vec<(Id, Id, Id)> = find!(
                (fact: Id, modality: Id, direction: Id),
                pattern!(facts, [
                    { node @ archive_schema::block::holds: ?fact },
                    { ?fact @ archive_schema::content_fact::modality: ?modality,
                      archive_schema::content_fact::direction: ?direction },
                ])
            )
            .collect();
            let directions: BTreeSet<Id> = held.iter().map(|(_, _, d)| *d).collect();
            let direction = match directions.iter().collect::<Vec<_>>().as_slice() {
                [d] if **d == IN => Direction::Received,
                [d] if **d == OUT => Direction::Produced,
                _ => Direction::Unknown,
            };
            let withheld = stopped.contains(&node);
            let mut first_input = None;
            let mut parts = Vec::new();
            for (fact, modality, direction) in held {
                let kind = label(v3_fact::modality::SPECS, modality);
                if withheld {
                    continue;
                }
                let texts: Vec<TextHandle> = find!(
                    text: TextHandle,
                    pattern!(facts, [{ fact @ archive_schema::content_fact::payload: ?text }])
                )
                .collect();
                if direction == IN && first_input.is_none() {
                    first_input = texts.first().copied();
                }
                for text in &texts {
                    parts.push(Part {
                        kind: kind.clone(),
                        body: match read_text(reader, *text) {
                            Some(text) => Body::Text(text),
                            None => Body::Missing(format!(
                                "text {} is not resident here",
                                hex::encode(&text.raw[..8])
                            )),
                        },
                    });
                }
                if texts.is_empty() {
                    let blob = exists!(pattern!(facts, [{
                        fact @ archive_schema::content_fact::blob: _?blob
                    }]));
                    let pointer = exists!(pattern!(facts, [{
                        fact @ archive_schema::content_fact::asset_pointer: _?pointer
                    }]));
                    parts.push(Part {
                        kind: kind.clone(),
                        body: Body::Other(
                            match (blob, pointer) {
                                (true, _) => "bytes held, not shown as text",
                                (false, true) => "a pointer to bytes the source did not hold",
                                (false, false) => "no content recorded",
                            }
                            .to_owned(),
                        ),
                    });
                }
            }
            Turn {
                node,
                direction,
                roles,
                parts,
                answers,
                answered_by,
                withheld,
                first_input,
            }
        })
        .collect()
}

/// Whether the archived conversation opens with exactly the prepared input:
/// `Some(true)` when its first recorded input is the same text (the same
/// handle), `Some(false)` when it is a different text, `None` when either is
/// unknown. Equal handles are equal texts; nothing is compared by content.
pub fn opening_agrees(opening: &[TextHandle], first: &[Turn]) -> Option<bool> {
    let [prepared] = opening else {
        return None;
    };
    let received = first.iter().find_map(|turn| turn.first_input)?;
    Some(received == *prepared)
}

#[cfg(test)]
mod tests;
