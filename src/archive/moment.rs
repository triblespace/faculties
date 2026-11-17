//! The Moment: the newest raw turns the Archive holds for one selector,
//! rendered compactly under a character budget, for the end of a wake.
//!
//! It is a pure function of the pile, the selector and the budget. The
//! selector is always explicit: an agent name as the source's own session
//! metadata spells it (`raw_agent_name`), a session locator, or a thread. No
//! persona is read. A label only says which material is the newest held for
//! that name; it never claims to be the exact handoff, so the heading says
//! "latest held material", names its source and its time, and says so when
//! nothing is held.
//!
//! The walk is bounded twice: by a node count, which bounds the blobs read,
//! and by the rendered characters. What it cuts is marked where it was cut.

use super::*;

/// Which held material a Moment shows.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MomentSelector {
    /// The newest leaf whose receipt carries exactly this `raw_agent_name`.
    /// Codex subagent threads (and Claude Code subagent transcripts) are left
    /// out unless `subagents` asks for them.
    Label { label: String, subagents: bool },
    /// An explicit session edge: the leaves whose receipts carry this
    /// `source_locator` (`session/<id>`). Subagents as for a label.
    Session { locator: String, subagents: bool },
    /// An explicit thread edge: the leaves whose receipts carry
    /// `source_thread` `thread/<id>`.
    Thread(String),
}

impl MomentSelector {
    fn explicit(&self) -> bool {
        !matches!(self, Self::Label { .. })
    }

    fn name(&self) -> String {
        match self {
            Self::Label { label, .. } => label.clone(),
            Self::Session { locator, .. } => locator.clone(),
            Self::Thread(thread) => thread_locator(thread),
        }
    }
}

fn thread_locator(thread: &str) -> String {
    if thread.starts_with("thread/") {
        thread.to_owned()
    } else {
        format!("thread/{thread}")
    }
}

/// The Moment's budget and reader.
#[derive(Clone, Debug)]
pub struct MomentOptions {
    pub selector: MomentSelector,
    /// Rendered characters, the heading not counted.
    pub chars: usize,
    /// Nodes read at most, and so payloads read at most.
    pub max_nodes: usize,
    /// The provider the reader declares, as `archive --reader-provider`:
    /// what a recorded classifier stop withholds.
    pub provider: Option<String>,
}

impl MomentOptions {
    pub const DEFAULT_CHARS: usize = 30_000;
    pub const DEFAULT_MAX_NODES: usize = 600;

    pub fn new(selector: MomentSelector) -> Self {
        Self {
            selector,
            chars: Self::DEFAULT_CHARS,
            max_nodes: Self::DEFAULT_MAX_NODES,
            provider: None,
        }
    }
}

/// Characters a single tool call's input, tool result or event keeps.
const CALL_CHARS: usize = 400;
const RESULT_CHARS: usize = 600;
const EVENT_CHARS: usize = 200;
const CHANNEL_CHARS: usize = 2_000;
/// Below this many characters left, a node that does not fit is not cut to
/// fit: the walk stops before it.
const MIN_PARTIAL: usize = 200;

impl Archive {
    /// The Moment section for `options`, ending in a newline. Reads only:
    /// the Archive's attached fact views are maintained as every Archive read
    /// maintains them.
    pub fn moment(&self, options: &MomentOptions) -> Result<String> {
        self.moment_at(options, crate::clock::now()?)
    }

    pub(crate) fn moment_at(&self, options: &MomentOptions, now: Epoch) -> Result<String> {
        self.with_operation(|storage| {
            let observed = storage.load()?;
            let facts = crate::storage::acquire_attached_facts(&observed)?;
            render_moment(&facts, observed.snapshot(), options, now)
        })
    }
}

/// One leaf a selector names.
#[derive(Clone, Copy, Debug)]
struct Leaf {
    node: Id,
    receipt: Id,
    at: Option<(i128, i128)>,
    /// Another node follows it: the material goes on past it.
    continued: bool,
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

/// The (receipt, node) pairs the selector names, and how many subagent
/// receipts it left out.
fn selected_receipts(facts: &FactArchive, selector: &MomentSelector) -> (Vec<(Id, Id)>, usize) {
    let pairs: Vec<(Id, Id)> = match selector {
        MomentSelector::Label { label, .. } => {
            let label = label.as_str();
            find!(
                (receipt: Id, node: Id),
                pattern!(facts, [{
                    ?receipt @ archive_schema::source_projection::raw_agent_name: label,
                    archive_schema::source_projection::projects_to: ?node
                }])
            )
            .collect()
        }
        MomentSelector::Session { locator, .. } => {
            let locator = locator.as_str();
            find!(
                (receipt: Id, node: Id),
                pattern!(facts, [{
                    ?receipt @ archive_schema::source_projection::source_locator: locator,
                    archive_schema::source_projection::projects_to: ?node
                }])
            )
            .collect()
        }
        MomentSelector::Thread(thread) => {
            let thread = thread_locator(thread);
            let thread = thread.as_str();
            find!(
                (receipt: Id, node: Id),
                pattern!(facts, [{
                    ?receipt @ archive_schema::source_projection::source_thread: thread,
                    archive_schema::source_projection::projects_to: ?node
                }])
            )
            .collect()
        }
    };
    let subagents = match selector {
        MomentSelector::Label { subagents, .. } | MomentSelector::Session { subagents, .. } => {
            *subagents
        }
        MomentSelector::Thread(_) => true,
    };
    let total = pairs.len();
    let kept: Vec<(Id, Id)> = pairs
        .into_iter()
        .filter(|(receipt, _)| subagents || !subagent_receipt(facts, *receipt))
        .collect();
    let left_out = total - kept.len();
    (kept, left_out)
}

/// The newest leaf the selector names, preferring one nothing follows, and
/// how many subagent leaves it left out.
fn select_leaf(facts: &FactArchive, selector: &MomentSelector) -> (Option<Leaf>, usize) {
    let (pairs, left_out) = selected_receipts(facts, selector);
    let leaves: Vec<Leaf> = pairs
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
    let open = newest(&mut leaves.iter().copied().filter(|leaf| !leaf.continued));
    (
        open.or_else(|| newest(&mut leaves.iter().copied())),
        left_out,
    )
}

fn utc(tai_ns: i128) -> String {
    let epoch = Epoch::from_tai_duration(hifitime::Duration::from_total_nanoseconds(tai_ns));
    let (year, month, day, hour, minute, second, _) = epoch.to_gregorian_utc();
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn age(now: Epoch, tai_ns: i128) -> String {
    let now = now.to_tai_duration().total_nanoseconds();
    let seconds = (now - tai_ns) / 1_000_000_000;
    if seconds < 0 {
        return format!("{}s after this wake's clock", -seconds);
    }
    let (days, hours, minutes) = (
        seconds / 86_400,
        seconds % 86_400 / 3_600,
        seconds % 3_600 / 60,
    );
    if days > 0 {
        format!("{days}d {hours}h before this wake")
    } else if hours > 0 {
        format!("{hours}h {minutes}m before this wake")
    } else {
        format!("{minutes}m {}s before this wake", seconds % 60)
    }
}

/// `text` cut to `max` characters, with a marker saying how much was cut.
fn clip(text: &str, max: usize) -> String {
    let total = text.chars().count();
    if total <= max {
        return text.to_owned();
    }
    let kept: String = text.chars().take(max).collect();
    format!("{kept} [... cut: {} more chars]", total - max)
}

const STATUS_PREFIX: &str = "urn:triblespace:archive:tool-result-status:";

/// A tool call as its name and a clipped input.
fn render_call(payload: &str) -> String {
    let value: Option<serde_json::Value> = serde_json::from_str(payload).ok();
    let name = value
        .as_ref()
        .and_then(|value| value.get("name"))
        .and_then(|name| name.as_str());
    let input = value.as_ref().and_then(|value| {
        value
            .get("input")
            .or_else(|| value.get("arguments"))
            .map(|input| match input {
                serde_json::Value::String(text) => text.clone(),
                other => other.to_string(),
            })
    });
    match (name, input) {
        (Some(name), Some(input)) => format!("call {name}: {}", clip(&input, CALL_CHARS)),
        _ => format!("call: {}", clip(payload, CALL_CHARS)),
    }
}

/// An event: dropped if it is a tool-result status, the message text if it
/// is a channel message, otherwise clipped.
fn render_event(payload: &str) -> Option<String> {
    if payload.starts_with(STATUS_PREFIX) {
        return None;
    }
    let value: Option<serde_json::Value> = serde_json::from_str(payload).ok();
    let channel = value.as_ref().and_then(|value| {
        let attachment = value.get("attachment")?;
        let channel = attachment.get("origin")?.get("kind")?.as_str()? == "channel";
        channel
            .then(|| attachment.get("prompt")?.as_str())
            .flatten()
    });
    Some(match channel {
        Some(prompt) => format!("channel: {}", clip(prompt, CHANNEL_CHARS)),
        None => format!("event: {}", clip(payload, EVENT_CHARS)),
    })
}

/// One node, compactly: its facts, or the line saying why they are withheld.
fn render_node(
    facts: &FactArchive,
    reader: &PileSnapshot,
    node: Id,
    stopped: bool,
    reading: Reading<'_>,
) -> Result<String> {
    let mut out = String::new();
    if stopped {
        match guard(facts, reader, node, reading)? {
            Guard::Withhold(line) => return Ok(format!("{line}\n")),
            Guard::Note(line) => writeln!(out, "{line}")?,
            Guard::Clear => {}
        }
    }
    let held: BTreeSet<(Id, Id)> = find!(
        (fact: Id, modality: Id),
        pattern!(facts, [
            { node @ archive_schema::block::holds: ?fact },
            { ?fact @ archive_schema::content_fact::modality: ?modality },
        ])
    )
    .collect();
    use archive_schema::content_fact::modality as m;
    for (fact, modality) in held {
        let texts: Vec<String> = find!(
            text: TextHandle,
            pattern!(facts, [{ fact @ archive_schema::content_fact::payload: ?text }])
        )
        .map(|text| read_text(reader, text))
        .collect::<Result<_>>()?;
        let has_bytes = exists!(pattern!(facts, [{
            fact @ archive_schema::content_fact::blob: _?blob
        }])) || exists!(pattern!(facts, [{
            fact @ archive_schema::content_fact::asset_pointer: _?pointer
        }]));
        if modality == m::TEXT {
            for text in &texts {
                writeln!(out, "{}", text.trim_end())?;
            }
        } else if modality == m::THINKING {
            for text in &texts {
                writeln!(out, "(thinking) {}", text.trim_end())?;
            }
            if has_bytes {
                writeln!(out, "[sealed thinking]")?;
            }
        } else if modality == m::TOOL_CALL {
            for text in &texts {
                writeln!(out, "{}", render_call(text))?;
            }
        } else if modality == m::TOOL_RESULT {
            for text in &texts {
                writeln!(out, "result: {}", clip(text.trim_end(), RESULT_CHARS))?;
            }
            if has_bytes && texts.is_empty() {
                writeln!(out, "result: [bytes not shown]")?;
            }
        } else if modality == m::EVENT {
            for text in &texts {
                if let Some(line) = render_event(text) {
                    writeln!(out, "{line}")?;
                }
            }
        } else {
            let label = m::SPECS
                .iter()
                .find(|(id, _)| *id == modality)
                .map_or("media", |(_, label)| label);
            writeln!(out, "[{label}]")?;
        }
    }
    Ok(out)
}

fn direction_of(facts: &FactArchive, node: Id) -> &'static str {
    if exists!(pattern!(facts, [
        { node @ archive_schema::block::holds: _?fact },
        { _?fact @ archive_schema::content_fact::direction:
            archive_schema::content_fact::direction::OUT },
    ])) {
        "out"
    } else {
        "in"
    }
}

/// Why the walk ended.
#[derive(Debug, Eq, PartialEq)]
enum Ended {
    /// It reached the conversation's start.
    Whole,
    Budget,
    Nodes,
}

fn render_moment(
    facts: &FactArchive,
    reader: &PileSnapshot,
    options: &MomentOptions,
    now: Epoch,
) -> Result<String> {
    let selector = &options.selector;
    let name = selector.name();
    let mut out = String::new();
    let (leaf, left_out) = select_leaf(facts, selector);
    let Some(leaf) = leaf else {
        writeln!(out, "Moment: no held material for {name}")?;
        let what = match selector {
            MomentSelector::Label { .. } => format!(
                "No Archive receipt carries the agent name {name:?}. Only imports made since the \
                 importer keeps the source's agent names carry one."
            ),
            MomentSelector::Session { .. } => {
                format!("No Archive receipt carries the source locator {name:?}.")
            }
            MomentSelector::Thread(_) => {
                format!("No Archive receipt carries the source thread {name:?}.")
            }
        };
        writeln!(out, "{what}")?;
        if left_out > 0 {
            writeln!(
                out,
                "({left_out} subagent leaf receipt(s) carry it and are left out; ask for subagents to see them.)"
            )?;
        }
        return Ok(out);
    };

    // The nodes a recorded classifier stop followed, found once.
    let stopped = classifier_stopped(facts);
    let reading = Reading {
        force: false,
        provider: options.provider.as_deref(),
    };

    // Newest first, breadth-first along `previous`, as `thread --tail`
    // walks: at most `max_nodes` nodes read, and the walk stops once the
    // rendered nodes fill the budget.
    let mut parents = BTreeMap::<Id, BTreeSet<Id>>::new();
    let mut rendered = BTreeMap::<Id, String>::new();
    let mut used = 0usize;
    let mut ended = Ended::Whole;
    let mut step = BTreeSet::from([leaf.node]);
    'walk: while !step.is_empty() {
        let mut next = BTreeSet::new();
        for node in step {
            if parents.contains_key(&node) {
                continue;
            }
            if parents.len() == options.max_nodes {
                ended = Ended::Nodes;
                break 'walk;
            }
            let mut text = render_node(facts, reader, node, stopped.contains(&node), reading)?;
            let length = text.chars().count();
            if used + length > options.chars {
                let left = options.chars.saturating_sub(used);
                if left < MIN_PARTIAL && !parents.is_empty() {
                    ended = Ended::Budget;
                    break 'walk;
                }
                // Keep the newest part of a node too long for what is left.
                let skip = length - left.min(length);
                let tail: String = text.chars().skip(skip).collect();
                text = format!("[... cut: {skip} earlier chars of this node]\n{tail}");
                if !tail.ends_with('\n') {
                    text.push('\n');
                }
                used = options.chars;
                let previous: BTreeSet<Id> = find!(
                    previous: Id,
                    pattern!(facts, [{ node @ archive_schema::block::previous: ?previous }])
                )
                .collect();
                parents.insert(node, previous);
                rendered.insert(node, text);
                ended = Ended::Budget;
                break 'walk;
            }
            used += length;
            let previous: BTreeSet<Id> = find!(
                previous: Id,
                pattern!(facts, [{ node @ archive_schema::block::previous: ?previous }])
            )
            .collect();
            next.extend(previous.iter().copied());
            parents.insert(node, previous);
            rendered.insert(node, text);
        }
        step = next;
    }
    let older = parents
        .values()
        .flatten()
        .any(|parent| !parents.contains_key(parent));
    let order = reading_order(&parents)?;

    // Heading.
    let edge = if selector.explicit() {
        format!("Moment: the predecessor named explicitly as {name}; latest held material for it")
    } else {
        format!("Moment: latest held material for {name}")
    };
    writeln!(out, "{edge}")?;
    let locators: BTreeSet<String> = find!(
        value: TextHandle,
        pattern!(facts, [{
            leaf.receipt @ archive_schema::source_projection::source_locator: ?value
        }])
    )
    .chain(find!(
        value: TextHandle,
        pattern!(facts, [{
            leaf.receipt @ archive_schema::source_projection::source_thread: ?value
        }])
    ))
    .map(|value| read_text(reader, value))
    .collect::<Result<_>>()?;
    writeln!(
        out,
        "source: {} (leaf node {:X})",
        if locators.is_empty() {
            "no source locator".to_owned()
        } else {
            locators.into_iter().collect::<Vec<_>>().join(", ")
        },
        leaf.node
    )?;
    match leaf.at {
        Some((_, upper)) => writeln!(
            out,
            "newest held material is from {} ({})",
            utc(upper),
            age(now, upper)
        )?,
        None => writeln!(out, "newest held material carries no time")?,
    }
    if leaf.continued {
        writeln!(
            out,
            "note: every node {name} names has a successor; the material goes on past what it names"
        )?;
    }
    writeln!(
        out,
        "shown: {} node(s), {used} chars; {}",
        order.len(),
        match (&ended, older) {
            (_, false) => "the whole conversation".to_owned(),
            (Ended::Nodes, true) => format!(
                "older material not shown: the walk stopped at {} nodes",
                options.max_nodes
            ),
            (_, true) => format!(
                "older material not shown: the walk stopped at the {}-character budget",
                options.chars
            ),
        }
    )?;
    writeln!(out, "---")?;
    if older {
        writeln!(out, "[... older material not shown]")?;
    }

    // Body: one label per run of one direction.
    let mut last = None;
    for node in order {
        let direction = direction_of(facts, node);
        if last != Some(direction) {
            let at = node_time(facts, node, None)
                .map(|(lower, _)| format!(" {}", utc(lower)))
                .unwrap_or_default();
            writeln!(out, "[{direction}{at}]")?;
            last = Some(direction);
        }
        out.push_str(&rendered[&node]);
    }
    Ok(out)
}
