//! A conversation named by its source locator, `session/<id>` or
//! `thread/<id>`, in place of a node id: `archive thread` then walks back
//! from that conversation's newest held leaf. This is how a new window reads
//! its retired predecessor's tail, given the session the launcher names.
//!
//! A native zooid's history has no importer receipt: Drive names the zooid on
//! each context it lives (`context::zooid`). `zooid/<name>` walks back from
//! where that history ends, found exactly as a relaunch of the zooid finds it
//! ([`crate::spawn::continuation::zooid_tail`]), refusals included.
//!
//! A locator says which conversation, never how far it was imported: the
//! line printed before the thread names the leaf found and its time, and
//! says when other nodes follow it.

use super::*;
use crate::schemas::archive_v3 as v3;

/// A conversation as its source locates it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Locator {
    /// The leaves whose receipts carry this `source_locator`. A Codex
    /// subagent shares its root's session, so subagent threads are left out.
    Session(String),
    /// The leaves whose receipts carry this `source_thread`: exactly one
    /// thread, a subagent's included.
    Thread(String),
    /// The node a native zooid's archived history ends at, by the name its
    /// contexts carry.
    Zooid(String),
}

impl Locator {
    /// `session/<id>`, `thread/<id>` or `zooid/<name>`; anything else is a
    /// node or receipt prefix.
    pub(crate) fn parse(id: &str) -> Option<Self> {
        if id
            .strip_prefix("session/")
            .is_some_and(|rest| !rest.is_empty())
        {
            Some(Self::Session(id.to_owned()))
        } else if id
            .strip_prefix("thread/")
            .is_some_and(|rest| !rest.is_empty())
        {
            Some(Self::Thread(id.to_owned()))
        } else if id
            .strip_prefix("zooid/")
            .is_some_and(|rest| !rest.is_empty())
        {
            Some(Self::Zooid(id.to_owned()))
        } else {
            None
        }
    }

    fn name(&self) -> &str {
        match self {
            Self::Session(locator) | Self::Thread(locator) | Self::Zooid(locator) => locator,
        }
    }
}

/// One leaf a locator names.
#[derive(Clone, Copy, Debug)]
struct Leaf {
    node: Id,
    receipt: Id,
    at: Option<(i128, i128)>,
    /// Another node follows it: the material goes on past it.
    continued: bool,
}

/// The times the occurrences of `node`'s witnesses claim: where a native
/// runtime (Drive) puts a node's time.
pub(crate) fn occurrence_times(facts: &FactArchive, node: Id) -> Vec<(i128, i128)> {
    find!(
        value: (i128, i128),
        pattern!(facts, [
            { _?witness @ v3::witness::projects_to: node },
            { _?occurrence @ v3::occurrence::of: _?witness, v3::witness::timestamp: ?value }
        ])
    )
    .collect()
}

fn node_time(facts: &FactArchive, node: Id, receipt: Option<Id>) -> Option<(i128, i128)> {
    find!(
        value: (i128, i128),
        pattern!(facts, [{ node @ archive_schema::block::timestamp: ?value }])
    )
    .max_by_key(|(_, upper)| *upper)
    .or_else(|| {
        let receipt = receipt?;
        find!(
            value: (i128, i128),
            pattern!(facts, [{
                receipt @ archive_schema::source_projection::source_timestamp: ?value
            }])
        )
        .max_by_key(|(_, upper)| *upper)
    })
    .or_else(|| {
        occurrence_times(facts, node)
            .into_iter()
            .max_by_key(|(_, upper)| *upper)
    })
}

/// Whether a receipt ends a subagent's conversation, as its source says:
/// Codex `thread_source` `subagent`, or a Claude Code subagent's
/// `.meta.json` (`source_detail`).
fn subagent_receipt(facts: &FactArchive, receipt: Id) -> bool {
    exists!(pattern!(facts, [{
        receipt @ archive_schema::source_projection::raw_thread_source: "subagent"
    }])) || exists!(pattern!(facts, [{
        receipt @ archive_schema::source_projection::source_detail: _?detail
    }]))
}

/// The (receipt, node) pairs the locator names.
fn selected_receipts(facts: &FactArchive, locator: &Locator) -> Vec<(Id, Id)> {
    let name = locator.name();
    match locator {
        Locator::Session(_) => find!(
            (receipt: Id, node: Id),
            pattern!(facts, [{
                ?receipt @ archive_schema::source_projection::source_locator: name,
                archive_schema::source_projection::projects_to: ?node
            }])
        )
        .filter(|(receipt, _)| !subagent_receipt(facts, *receipt))
        .collect(),
        Locator::Thread(_) => find!(
            (receipt: Id, node: Id),
            pattern!(facts, [{
                ?receipt @ archive_schema::source_projection::source_thread: name,
                archive_schema::source_projection::projects_to: ?node
            }])
        )
        .collect(),
        Locator::Zooid(_) => unreachable!("a zooid is located by its contexts, not receipts"),
    }
}

/// The newest leaf the locator names, preferring one nothing follows.
fn select_leaf(facts: &FactArchive, locator: &Locator) -> Option<Leaf> {
    let leaves: Vec<Leaf> = selected_receipts(facts, locator)
        .into_iter()
        .map(|(receipt, node)| Leaf {
            node,
            receipt,
            at: node_time(facts, node, Some(receipt)),
            continued: exists!(pattern!(facts, [{
                _?next @ archive_schema::block::previous: node
            }])),
        })
        .collect();
    let newest = |leaves: &mut dyn Iterator<Item = Leaf>| {
        leaves.max_by_key(|leaf| (leaf.at.map(|(_, upper)| upper), leaf.node, leaf.receipt))
    };
    newest(&mut leaves.iter().copied().filter(|leaf| !leaf.continued))
        .or_else(|| newest(&mut leaves.iter().copied()))
}

fn utc(tai_ns: i128) -> String {
    let epoch = Epoch::from_tai_duration(hifitime::Duration::from_total_nanoseconds(tai_ns));
    let (year, month, day, hour, minute, second, _) = epoch.to_gregorian_utc();
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// The newest leaf `locator` names, and the line that says which it is.
pub(crate) fn conversation_leaf(facts: &FactArchive, locator: &Locator) -> Result<(Id, String)> {
    if let Locator::Zooid(_) = locator {
        return zooid_leaf(facts, locator);
    }
    let name = locator.name();
    let Some(leaf) = select_leaf(facts, locator) else {
        bail!("no Archive receipt carries {name}");
    };
    let mut line = format!("conversation: {name}, newest held leaf {:X}", leaf.node);
    match leaf.at {
        Some((_, upper)) => write!(line, " at {}", utc(upper))?,
        None => line.push_str(", which carries no time"),
    }
    if leaf.continued {
        line.push_str("; every leaf it names has a successor, so the material goes on past it");
    }
    Ok((leaf.node, line))
}

/// Where the zooid `zooid/<name>` names ends its archived history, and the
/// line that says so. A refusal of [`crate::spawn::continuation::zooid_tail`]
/// is the error, never an empty result.
fn zooid_leaf(facts: &FactArchive, locator: &Locator) -> Result<(Id, String)> {
    let locator = locator.name();
    let name = &locator["zooid/".len()..];
    let Some(node) = crate::spawn::continuation::zooid_tail(facts, name)? else {
        bail!("no archived context names {locator}");
    };
    let mut line = format!("zooid: {name}, history ends at {node:X}");
    match node_time(facts, node, None) {
        Some((_, upper)) => write!(line, " at {}", utc(upper))?,
        None => line.push_str(", which carries no time"),
    }
    if exists!(pattern!(facts, [{ _?next @ archive_schema::block::previous: node }])) {
        line.push_str("; a node outside the zooid's own contexts follows it");
    }
    Ok((node, line))
}
