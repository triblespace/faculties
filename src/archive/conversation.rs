//! A conversation named by its source locator, `session/<id>` or
//! `thread/<id>`, in place of a node id: `archive thread` then walks back
//! from that conversation's newest held leaf. This is how a new window reads
//! its retired predecessor's tail, given the session the launcher names.
//!
//! A locator says which conversation, never how far it was imported: the
//! line printed before the thread names the leaf found and its time, and
//! says when other nodes follow it.

use super::*;

/// A conversation as its source locates it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Locator {
    /// The leaves whose receipts carry this `source_locator`. A Codex
    /// subagent shares its root's session, so subagent threads are left out.
    Session(String),
    /// The leaves whose receipts carry this `source_thread`: exactly one
    /// thread, a subagent's included.
    Thread(String),
}

impl Locator {
    /// `session/<id>` or `thread/<id>`; anything else is a node or receipt
    /// prefix.
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
        } else {
            None
        }
    }

    fn name(&self) -> &str {
        match self {
            Self::Session(locator) | Self::Thread(locator) => locator,
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

fn node_time(facts: &FactArchive, node: Id, receipt: Id) -> Option<(i128, i128)> {
    find!(
        value: (i128, i128),
        pattern!(facts, [{ node @ archive_schema::block::timestamp: ?value }])
    )
    .max_by_key(|(_, upper)| *upper)
    .or_else(|| {
        find!(
            value: (i128, i128),
            pattern!(facts, [{
                receipt @ archive_schema::source_projection::source_timestamp: ?value
            }])
        )
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
    }
}

/// The newest leaf the locator names, preferring one nothing follows.
fn select_leaf(facts: &FactArchive, locator: &Locator) -> Option<Leaf> {
    let leaves: Vec<Leaf> = selected_receipts(facts, locator)
        .into_iter()
        .map(|(receipt, node)| Leaf {
            node,
            receipt,
            at: node_time(facts, node, receipt),
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
