//! B: the gate's independent walk over a source unit.
//!
//! It parses the bytes with its own minimal RFC 8259 parser (not the
//! TribleSpace scanner, not an importer's parser), walks every value under
//! its normalized path, and lets the importer's [`FieldTable`] say what the
//! value is. It emits a [`Token`] for each fact and annotation value, keeps
//! the source's order (record parent links, array order within a record),
//! and asserts drops: a mirror must equal what it mirrors, an empty drop must
//! be empty. Anything the table does not name is a row, never a skip.
//!
//! Object members are visited in name order, never in the order the source
//! spelled them: member order carries no information.
//!
//! It also groups the fact tokens into *items* (the tokens of one source
//! occurrence: one array element, or the record itself) and builds, for each
//! record, the window behind it: the record chain by the order rule, opened
//! by the prefix in force and shaped by the window edits the source marks
//! ([`Windows`]). The gate compares every witnessed window against these,
//! occurrence by occurrence.
//!
//! A path segment that looks like an id (a uuid, a hex or numeric key, a
//! prefixed provider id) prints as `<id>`, so a report row never holds one.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::str::FromStr;

use base64::Engine;
use hifitime::{Duration, Epoch};

use super::{id_like, JsonLeaf, Kind, Origin, Token, Value};
use crate::archive_v3::fields::{
    Class, Cond, Decode, DropReason, Entry, FieldTable, FileWindows, Mirror, Order, Structure,
    Windows,
};

/// What B found in one unit.
#[derive(Debug, Default)]
pub struct Walked {
    /// Every token, with every place it occurs.
    pub tokens: BTreeMap<Token, Vec<Origin>>,
    /// Findings: (path, kind, occurrence label). Labels only count rows.
    pub rows: Vec<(String, Kind, String)>,
    /// Every item: the fact tokens of one source occurrence.
    pub items: Vec<Item>,
    records: Vec<RecordLinks>,
    ids: HashMap<String, Vec<usize>>,
    windows: Option<Windows>,
    order: Option<Order>,
    /// Anchor id -> the first compaction record that names it.
    anchors: HashMap<String, usize>,
    /// Each record's prefix record: the latest one before it in the file.
    prefixes: Vec<Option<usize>>,
    /// [`Order::File`]: the record before each one that holds items.
    file_parent: Vec<Option<usize>>,
    /// [`Order::File`]: what precedes each record's items, as the opening
    /// (header and prefix items) and the line's last place in the arena.
    file_trails: Vec<FileTrail>,
    /// Places on a line: (item, previous place).
    file_arena: Vec<(usize, Option<usize>)>,
    /// Values at the paths unit mirrors name: path -> exact hashes, and the
    /// strings themselves.
    watched: HashMap<String, (std::collections::HashSet<[u8; 32]>, Vec<String>)>,
    /// Unit mirrors to check once the whole unit is read: (path, mirror,
    /// the value's hash, the string when it is one, occurrence label).
    pending: Vec<(String, Mirror, [u8; 32], Option<String>, String)>,
}

/// What precedes one record's items under [`Order::File`].
#[derive(Clone, Debug, Default)]
struct FileTrail {
    opening: Vec<usize>,
    tail: Option<usize>,
    /// A rollback the line could not take came before it.
    broken: bool,
}

/// What a record is under [`Order::File`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum FileKind {
    #[default]
    Content,
    Header,
    Prefix,
    Replace,
    Turn,
    /// How many turns, when the count is one.
    Rollback(Option<usize>),
}

/// The windows behind one record, as hashes: what precedes its own items
/// (`None` when the edits the source marks cannot be applied), the window a
/// child continues (`before`, its own items, a segment re-presented after
/// it), and the same history unedited, which a new request starts from.
#[derive(Clone, Copy, Debug)]
pub struct Lines {
    pub before: Option<[u8; 32]>,
    line: Option<[u8; 32]>,
    unedited: [u8; 32],
    prefix: Option<usize>,
    /// The settings header item the line opens with.
    header: Option<usize>,
}

/// The fact tokens of one source occurrence (one modality at one place),
/// and where it sits.
#[derive(Debug)]
pub struct Item {
    pub record: usize,
    /// `(array path, position)` from the outermost array in, empty for a
    /// value outside every array.
    pub levels: Vec<(String, usize)>,
    pub modality: String,
    pub tokens: BTreeSet<Token>,
    /// The normalized path of its first token.
    pub path: String,
}

#[derive(Debug, Default)]
struct RecordLinks {
    id: Option<String>,
    parent: Option<String>,
    /// A `Mirror::Children` drop: its path and the ids it lists.
    children: Option<(String, Vec<String>)>,
    /// The record's discriminator value.
    shape: Option<String>,
    /// The id of the request this record belongs to.
    request: Option<String>,
    /// `(head, tail, anchor)` of a compaction.
    compaction: Option<(String, String, String)>,
    /// Each dropped block's `messages.<i>.content.<j>` path, as written.
    dropped: Vec<String>,
    /// Each cleared count; `None` when it is no count.
    cleared: Vec<Option<usize>>,
    /// The levels of each result in the record, in walk order.
    results: Vec<Vec<(String, usize)>>,
    /// Its items, in record order.
    items: Vec<usize>,
    /// The request's settings header ([`Windows::header`]), an item that is
    /// none of its own.
    header: Option<usize>,
    /// Its kind under [`Order::File`].
    file: FileKind,
}

/// Who an item in a window speaks for, as the window's messages count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    User,
    Assistant,
    System,
    Voiced,
}

/// One place in a window: an item, or the head of a result that holds no
/// value of its own (it still counts as a result and as a block).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Slot {
    Item(usize),
    Head(usize, usize),
}

impl Walked {
    /// The first record with this id.
    fn first(&self, id: &str) -> Option<usize> {
        self.ids
            .get(id)
            .and_then(|records| records.first().copied())
    }

    /// Whether a record repeats an earlier record's id.
    pub fn is_repeat(&self, record: usize) -> bool {
        self.records[record]
            .id
            .as_deref()
            .and_then(|id| self.first(id))
            .is_some_and(|first| first != record)
    }

    /// The number of records.
    pub fn record_count(&self) -> usize {
        self.records.len()
    }

    /// The record's items, in record order.
    pub fn record_items(&self, record: usize) -> &[usize] {
        &self.records[record].items
    }

    /// The record's parent by the order rule, when the unit holds it.
    pub fn parent(&self, record: usize) -> Option<usize> {
        if let Some(Order::File(_)) = self.order {
            return self.file_parent.get(record).copied().flatten();
        }
        self.records[record]
            .parent
            .as_deref()
            .and_then(|parent| self.first(parent))
    }

    fn role(&self, slot: Slot) -> Role {
        let Slot::Item(item) = slot else {
            return Role::User;
        };
        let tokens = &self.items[item].tokens;
        if tokens
            .iter()
            .any(|t| t.attribute == "form" && t.value == Value::Term("voiced".to_owned()))
        {
            Role::Voiced
        } else if tokens.iter().any(|t| t.modality == "system") {
            Role::System
        } else if tokens.iter().any(|t| t.direction == "out") {
            Role::Assistant
        } else {
            Role::User
        }
    }

    /// The record's slots in record order: its items, and a head for each
    /// result whose head holds no value. A result's head comes before its
    /// parts.
    fn slots(&self, record: usize) -> Vec<(Vec<(String, usize)>, Slot)> {
        let links = &self.records[record];
        let mut slots: Vec<(Vec<(String, usize)>, bool, Slot)> = links
            .items
            .iter()
            .map(|&item| {
                let part = self
                    .result_of(record, Slot::Item(item))
                    .is_some_and(|(_, part)| part);
                (self.items[item].levels.clone(), part, Slot::Item(item))
            })
            .collect();
        for (group, levels) in links.results.iter().enumerate() {
            let headed = links
                .items
                .iter()
                .any(|&item| self.result_of(record, Slot::Item(item)) == Some((group, false)));
            if !headed {
                slots.push((levels.clone(), false, Slot::Head(record, group)));
            }
        }
        slots.sort();
        slots
            .into_iter()
            .map(|(levels, _, slot)| (levels, slot))
            .collect()
    }

    /// The result a slot belongs to, and whether it is one of its parts.
    fn result_of(&self, record: usize, slot: Slot) -> Option<(usize, bool)> {
        let item = match slot {
            Slot::Item(item) => item,
            Slot::Head(_, group) => return Some((group, false)),
        };
        let head = self.windows?.requests?.result.1;
        let levels = &self.items[item].levels;
        let results = &self.records[record].results;
        results
            .iter()
            .position(|group| levels.starts_with(group))
            .map(|group| {
                let part = levels.len() > results[group].len() || self.items[item].modality != head;
                (group, part)
            })
    }

    /// The slots of `records` in window order, each followed by the
    /// segment a compaction re-presents after it. `None` when a segment
    /// does not reach its head.
    fn walk_records(&self, records: &[usize]) -> Option<Vec<(usize, Vec<(String, usize)>, Slot)>> {
        let mut out = Vec::new();
        for &record in records {
            out.extend(
                self.slots(record)
                    .into_iter()
                    .map(|(levels, slot)| (record, levels, slot)),
            );
            if let Some(segment) = self.segment_after(record) {
                for at in segment? {
                    out.extend(
                        self.slots(at)
                            .into_iter()
                            .map(|(levels, slot)| (at, levels, slot)),
                    );
                }
            }
        }
        Some(out)
    }

    /// The segment a compaction re-presents after `record`, root first:
    /// `Some(None)` when it does not reach its head.
    fn segment_after(&self, record: usize) -> Option<Option<Vec<usize>>> {
        let id = self.records[record].id.as_deref()?;
        let boundary = *self.anchors.get(id)?;
        let (head, tail, _) = self.records[boundary].compaction.clone()?;
        let mut chain = Vec::new();
        let mut seen = BTreeSet::new();
        let mut cursor = self.first(&tail);
        loop {
            let Some(at) = cursor else {
                return Some(None);
            };
            if !seen.insert(at) {
                return Some(None);
            }
            chain.push(at);
            if self.records[at].id.as_deref() == Some(head.as_str()) {
                break;
            }
            cursor = self.parent(at);
        }
        chain.reverse();
        Some(Some(chain))
    }

    /// The settings header a chain's window opens with: the header of the
    /// first record of the latest request run on it.
    fn header(&self, chain: &[usize]) -> Option<usize> {
        let request = |at: usize| self.records[at].request.as_deref();
        let end = chain.iter().rposition(|&at| request(at).is_some())?;
        let mut start = end;
        while start > 0 && request(chain[start - 1]) == request(chain[end]) {
            start -= 1;
        }
        self.records[chain[start]].header
    }

    /// The window behind `record`: the items before its own, and its own,
    /// as item indices. `None` when the edits the source marks cannot be
    /// applied to it (a row says where).
    pub fn window(&self, record: usize) -> Option<(Vec<usize>, Vec<usize>)> {
        if let Some(Order::File(_)) = self.order {
            return self.file_window(record);
        }
        let ordered = |at: usize| -> Vec<usize> {
            self.slots(at)
                .into_iter()
                .filter_map(|(_, slot)| match slot {
                    Slot::Item(item) => Some(item),
                    Slot::Head(..) => None,
                })
                .collect()
        };
        let own = ordered(record);
        let windows = self.windows.unwrap_or(Windows::NONE);
        let shape_of = |at: usize| self.records[at].shape.as_deref();
        let is_prefix =
            |at: usize| shape_of(at).is_some_and(|shape| windows.prefix.contains(&shape));
        if is_prefix(record) {
            return Some((Vec::new(), own));
        }
        // The chain, root first.
        let mut chain = vec![record];
        let mut seen = BTreeSet::from([record]);
        let mut cursor = record;
        while let Some(up) = self.parent(cursor) {
            if !seen.insert(up) {
                break;
            }
            chain.push(up);
            cursor = up;
        }
        chain.reverse();
        let prefix: Vec<usize> = self.prefixes[record].map(ordered).unwrap_or_default();
        // The latest request on the chain, and its own edits so far.
        let request = |at: usize| self.records[at].request.as_deref();
        let (start, dropped, cleared) = match chain.iter().rposition(|&at| request(at).is_some()) {
            None => (chain.len() - 1, Vec::new(), Vec::new()),
            Some(end) => {
                let mut start = end;
                while start > 0 && request(chain[start - 1]) == request(chain[end]) {
                    start -= 1;
                }
                let run = &chain[start..=end];
                let dropped: Vec<String> = run
                    .iter()
                    .flat_map(|&at| self.records[at].dropped.iter().cloned())
                    .collect();
                let cleared: Vec<Option<usize>> = run
                    .iter()
                    .flat_map(|&at| self.records[at].cleared.iter().copied())
                    .collect();
                (start, dropped, cleared)
            }
        };
        let mut base = self.walk_records(&chain[..start])?;
        let tail = self.walk_records(&chain[start..chain.len() - 1])?;
        // Dropped blocks, by message and block in the base window.
        let mut drop = BTreeSet::new();
        if !dropped.is_empty() {
            let thinking = windows.requests.map(|r| r.thinking).unwrap_or_default();
            let mut runs: Vec<(Role, Vec<usize>)> = Vec::new();
            for (at, (_, _, slot)) in base.iter().enumerate() {
                let role = self.role(*slot);
                match runs.last_mut() {
                    Some((last, members)) if *last == role => members.push(at),
                    _ => runs.push((role, vec![at])),
                }
            }
            for place in &dropped {
                let steps: Vec<&str> = place.split('.').collect();
                let (message, block) = match steps.as_slice() {
                    ["messages", message, "content", block] => {
                        (message.parse::<usize>().ok()?, block.parse::<usize>().ok()?)
                    }
                    _ => return None,
                };
                if runs[..message.min(runs.len())]
                    .iter()
                    .any(|(role, _)| matches!(role, Role::System | Role::Voiced))
                {
                    return None;
                }
                let at = match runs.get(message) {
                    Some((Role::Assistant, members)) => *members.get(block)?,
                    _ => return None,
                };
                match base[at].2 {
                    Slot::Item(item)
                        if self.items[item]
                            .tokens
                            .iter()
                            .any(|t| t.modality == thinking) =>
                    {
                        drop.insert(at);
                    }
                    _ => return None,
                }
            }
        }
        // Cleared results: the oldest ones lose their parts.
        let mut count = 0;
        for cleared in &cleared {
            count += (*cleared)?;
        }
        let mut groups: Vec<(usize, usize)> = Vec::new();
        for (record, _, slot) in &base {
            if let Some((group, _)) = self.result_of(*record, *slot) {
                if !groups.contains(&(*record, group)) {
                    groups.push((*record, group));
                }
            }
        }
        if count > groups.len() {
            return None;
        }
        let cleared: BTreeSet<(usize, usize)> = groups.into_iter().take(count).collect();
        let mut at = 0;
        base.retain(|(record, _, slot)| {
            let keep = !drop.contains(&at)
                && !self
                    .result_of(*record, *slot)
                    .is_some_and(|(group, part)| part && cleared.contains(&(*record, group)));
            at += 1;
            keep
        });
        let items = |slots: Vec<(usize, Vec<(String, usize)>, Slot)>| {
            slots.into_iter().filter_map(|(_, _, slot)| match slot {
                Slot::Item(item) => Some(item),
                Slot::Head(..) => None,
            })
        };
        let mut before: Vec<usize> = self.header(&chain).into_iter().collect();
        before.extend(prefix);
        before.extend(items(base));
        before.extend(items(tail));
        Some((before, own))
    }

    /// The items a compaction re-presents after `record`, in order.
    fn segment_items(&self, record: usize) -> Option<Vec<usize>> {
        let mut out = Vec::new();
        if let Some(segment) = self.segment_after(record) {
            for at in segment? {
                out.extend(self.ordered(at));
            }
        }
        Some(out)
    }

    /// The record's items in window order.
    pub fn ordered(&self, record: usize) -> Vec<usize> {
        self.slots(record)
            .into_iter()
            .filter_map(|(_, slot)| match slot {
                Slot::Item(item) => Some(item),
                Slot::Head(..) => None,
            })
            .collect()
    }

    /// The windows behind every record, as hashes that `step` extends by one
    /// item id. A record continues its parent's line, or starts a request
    /// from its parent's unedited history, without walking the chain again;
    /// only a record whose prefix differs from its parent's, or a request
    /// whose own edits change the window, is built from [`Self::window`].
    pub fn lines(
        &self,
        id: &dyn Fn(usize) -> u32,
        step: &dyn Fn(&[u8; 32], u32) -> [u8; 32],
    ) -> Vec<Option<Lines>> {
        if let Some(Order::File(_)) = self.order {
            return self.file_lines(id, step);
        }
        let fold = |hash: [u8; 32], items: &[usize]| {
            items.iter().fold(hash, |hash, &item| step(&hash, id(item)))
        };
        let mut lines: Vec<Option<Lines>> = vec![None; self.records.len()];
        let mut busy = vec![false; self.records.len()];
        for start in 0..self.records.len() {
            // Parents first: climb to a record already done, then come down.
            let mut stack = vec![start];
            while let Some(&record) = stack.last() {
                if lines[record].is_some() {
                    stack.pop();
                    continue;
                }
                let parent = self.parent(record).filter(|&p| p != record);
                if let Some(parent) = parent {
                    if lines[parent].is_none() && !busy[parent] {
                        busy[record] = true;
                        stack.push(parent);
                        continue;
                    }
                }
                stack.pop();
                busy[record] = false;
                lines[record] = Some(self.line_of(record, parent.and_then(|p| lines[p]), &fold));
            }
        }
        lines
    }

    /// [`Order::File`]: the window behind `record`, from the trail the
    /// walk left ([`Walker::trails`]).
    fn file_window(&self, record: usize) -> Option<(Vec<usize>, Vec<usize>)> {
        let trail = self.file_trails.get(record)?;
        if trail.broken {
            return None;
        }
        let mut line = Vec::new();
        let mut cursor = trail.tail;
        while let Some(at) = cursor {
            line.push(self.file_arena[at].0);
            cursor = self.file_arena[at].1;
        }
        line.reverse();
        let mut before = trail.opening.clone();
        before.extend(line);
        Some((before, self.ordered(record)))
    }

    /// [`Order::File`]: the hash of the window behind every record, in one
    /// pass.
    fn file_lines(
        &self,
        id: &dyn Fn(usize) -> u32,
        step: &dyn Fn(&[u8; 32], u32) -> [u8; 32],
    ) -> Vec<Option<Lines>> {
        let fold = |hash: [u8; 32], items: &[usize]| {
            items.iter().fold(hash, |hash, &item| step(&hash, id(item)))
        };
        let lines_of = |before: Option<[u8; 32]>| Lines {
            before,
            line: None,
            unedited: [0; 32],
            prefix: None,
            header: None,
        };
        let mut out = Vec::with_capacity(self.records.len());
        let mut header: Vec<usize> = Vec::new();
        let mut prefix: Vec<usize> = Vec::new();
        let mut line: Vec<usize> = Vec::new();
        // The hash after each item of the line.
        let mut hashes: Vec<[u8; 32]> = Vec::new();
        let mut base = [0u8; 32];
        let mut turns: Vec<usize> = Vec::new();
        let mut broken = false;
        let rehash = |base: [u8; 32], line: &[usize], hashes: &mut Vec<[u8; 32]>| {
            hashes.clear();
            let mut hash = base;
            for &item in line {
                hash = step(&hash, id(item));
                hashes.push(hash);
            }
        };
        for at in 0..self.records.len() {
            let own = self.ordered(at);
            let tip = hashes.last().copied().unwrap_or(base);
            match self.records[at].file {
                FileKind::Header => {
                    out.push(Some(lines_of(Some([0; 32]))));
                    header = own;
                }
                FileKind::Prefix => {
                    out.push(Some(lines_of(Some(fold([0; 32], &header)))));
                    prefix = own;
                }
                FileKind::Replace => {
                    out.push(Some(lines_of(Some(base))));
                    line = own;
                    turns.clear();
                    broken = false;
                    rehash(base, &line, &mut hashes);
                }
                FileKind::Turn => {
                    out.push(Some(lines_of((!broken).then_some(tip))));
                    turns.push(line.len());
                }
                FileKind::Rollback(count) => {
                    out.push(Some(lines_of((!broken).then_some(tip))));
                    match count {
                        Some(0) => {}
                        Some(count) if count <= turns.len() && !broken => {
                            let keep = turns.len() - count;
                            line.truncate(turns[keep]);
                            hashes.truncate(turns[keep]);
                            turns.truncate(keep);
                        }
                        _ => broken = true,
                    }
                }
                FileKind::Content => {
                    out.push(Some(lines_of((!broken).then_some(tip))));
                    let mut hash = tip;
                    for item in own {
                        hash = step(&hash, id(item));
                        line.push(item);
                        hashes.push(hash);
                    }
                }
            }
            if matches!(self.records[at].file, FileKind::Header | FileKind::Prefix) {
                let opened = fold(fold([0; 32], &header), &prefix);
                if opened != base {
                    base = opened;
                    rehash(base, &line, &mut hashes);
                }
            }
        }
        out
    }

    fn line_of(
        &self,
        record: usize,
        parent: Option<Lines>,
        fold: &dyn Fn([u8; 32], &[usize]) -> [u8; 32],
    ) -> Lines {
        let links = &self.records[record];
        let own = self.ordered(record);
        let segment = self.segment_items(record);
        let prefix = self.prefixes[record];
        let marks = !links.dropped.is_empty() || !links.cleared.is_empty();
        let parent_links = self.parent(record).map(|p| &self.records[p]);
        let continues_request =
            links.request.is_some() && parent_links.is_some_and(|p| p.request == links.request);
        let new_request = links.request.is_some() && !continues_request;
        // The header this record's line opens with: its own when it starts
        // a request, else its parent's line's.
        let header = if new_request {
            links.header
        } else {
            parent.and_then(|parent| parent.header)
        };
        let quick = match parent {
            Some(parent) if parent.prefix == prefix && parent.header == header && !marks => {
                if new_request {
                    Some(parent.unedited)
                } else {
                    parent.line
                }
            }
            _ => None,
        };
        let windows = self.windows.unwrap_or(Windows::NONE);
        let is_prefix = links
            .shape
            .as_deref()
            .is_some_and(|shape| windows.prefix.contains(&shape));
        let (before, unedited) = match (quick, parent) {
            (Some(before), Some(parent)) if !is_prefix => {
                (Some(before), fold(parent.unedited, &own))
            }
            _ if self.is_repeat(record) => (None, [0; 32]),
            _ => {
                let before = self
                    .window(record)
                    .map(|(before, _)| fold([0; 32], &before));
                let unedited = if is_prefix {
                    fold([0; 32], &own)
                } else {
                    let mut chain = vec![record];
                    let mut seen = BTreeSet::from([record]);
                    let mut cursor = record;
                    while let Some(up) = self.parent(cursor) {
                        if !seen.insert(up) {
                            break;
                        }
                        chain.push(up);
                        cursor = up;
                    }
                    chain.reverse();
                    let mut items: Vec<usize> = self.header(&chain).into_iter().collect();
                    items.extend(prefix.map(|p| self.ordered(p)).unwrap_or_default());
                    for (at, &link) in chain.iter().enumerate() {
                        items.extend(self.ordered(link));
                        if at + 1 < chain.len() {
                            items.extend(self.segment_items(link).unwrap_or_default());
                        }
                    }
                    fold([0; 32], &items)
                };
                (before, unedited)
            }
        };
        let segment_hash = |hash: [u8; 32]| segment.as_ref().map(|items| fold(hash, items));
        let line = before.and_then(|before| segment_hash(fold(before, &own)));
        let unedited = segment_hash(unedited).unwrap_or(unedited);
        Lines {
            before,
            line,
            unedited,
            prefix,
            header,
        }
    }
}

/// Walk one unit's bytes under `table`.
pub fn walk(table: &FieldTable, source: &[u8]) -> Walked {
    let mut walker = Walker {
        table,
        out: Walked {
            windows: Some(table.windows),
            order: Some(table.order),
            ..Walked::default()
        },
        index: HashMap::new(),
        by_path: HashMap::new(),
    };
    for entry in table.entries {
        walker.by_path.entry(entry.path).or_default().push(entry);
    }
    walker.watch();
    if table.jsonl {
        for (line, bytes) in source.split(|byte| *byte == b'\n').enumerate() {
            if bytes.iter().all(|byte| byte.is_ascii_whitespace()) {
                continue;
            }
            walker.document(bytes, line + 1);
        }
    } else {
        walker.document(source, 0);
    }
    walker.finish();
    walker.out
}

// ---------------------------------------------------------------------------
// The parser.

#[derive(Clone, Debug)]
enum J {
    Null,
    Bool(bool),
    Num(String),
    Str(String),
    Arr(Vec<J>),
    /// Members in name order; names are unique.
    Obj(BTreeMap<String, J>),
}

const MAX_DEPTH: usize = 512;

struct Parser<'a> {
    bytes: &'a [u8],
    at: usize,
    depth: usize,
}

impl<'a> Parser<'a> {
    fn document(bytes: &'a [u8]) -> Result<J, String> {
        let mut parser = Parser {
            bytes,
            at: 0,
            depth: 0,
        };
        let value = parser.value()?;
        parser.whitespace();
        if parser.at != bytes.len() {
            return Err(format!("trailing bytes at {}", parser.at));
        }
        Ok(value)
    }

    fn whitespace(&mut self) {
        while matches!(self.bytes.get(self.at), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    fn expect(&mut self, byte: u8) -> Result<(), String> {
        if self.peek() == Some(byte) {
            self.at += 1;
            Ok(())
        } else {
            Err(format!("expected {:?} at {}", byte as char, self.at))
        }
    }

    fn value(&mut self) -> Result<J, String> {
        self.whitespace();
        match self.peek() {
            Some(b'{') => self.nested(Self::object),
            Some(b'[') => self.nested(Self::array),
            Some(b'"') => self.string().map(J::Str),
            Some(b't') => self.literal(b"true", J::Bool(true)),
            Some(b'f') => self.literal(b"false", J::Bool(false)),
            Some(b'n') => self.literal(b"null", J::Null),
            Some(b'-' | b'0'..=b'9') => self.number().map(J::Num),
            _ => Err(format!("expected a value at {}", self.at)),
        }
    }

    fn nested(&mut self, parse: fn(&mut Self) -> Result<J, String>) -> Result<J, String> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err("nesting too deep".to_owned());
        }
        let value = parse(self);
        self.depth -= 1;
        value
    }

    fn literal(&mut self, word: &[u8], value: J) -> Result<J, String> {
        if self.bytes[self.at..].starts_with(word) {
            self.at += word.len();
            Ok(value)
        } else {
            Err(format!("bad literal at {}", self.at))
        }
    }

    fn object(&mut self) -> Result<J, String> {
        self.expect(b'{')?;
        let mut members = BTreeMap::new();
        self.whitespace();
        if self.peek() == Some(b'}') {
            self.at += 1;
            return Ok(J::Obj(members));
        }
        loop {
            self.whitespace();
            let name = self.string()?;
            self.whitespace();
            self.expect(b':')?;
            let value = self.value()?;
            if members.insert(name, value).is_some() {
                return Err(format!("duplicate member name before {}", self.at));
            }
            self.whitespace();
            match self.peek() {
                Some(b',') => self.at += 1,
                Some(b'}') => {
                    self.at += 1;
                    return Ok(J::Obj(members));
                }
                _ => return Err(format!("expected , or }} at {}", self.at)),
            }
        }
    }

    fn array(&mut self) -> Result<J, String> {
        self.expect(b'[')?;
        let mut elements = Vec::new();
        self.whitespace();
        if self.peek() == Some(b']') {
            self.at += 1;
            return Ok(J::Arr(elements));
        }
        loop {
            elements.push(self.value()?);
            self.whitespace();
            match self.peek() {
                Some(b',') => self.at += 1,
                Some(b']') => {
                    self.at += 1;
                    return Ok(J::Arr(elements));
                }
                _ => return Err(format!("expected , or ] at {}", self.at)),
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let digits = self
            .bytes
            .get(self.at..self.at + 4)
            .ok_or_else(|| "short \\u escape".to_owned())?;
        let text = std::str::from_utf8(digits).map_err(|_| "bad \\u escape".to_owned())?;
        if !text.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("bad \\u escape".to_owned());
        }
        self.at += 4;
        Ok(u32::from_str_radix(text, 16).expect("four hex digits"))
    }

    fn string(&mut self) -> Result<String, String> {
        self.expect(b'"')?;
        let mut out: Vec<u8> = Vec::new();
        loop {
            let Some(byte) = self.peek() else {
                return Err("unterminated string".to_owned());
            };
            self.at += 1;
            match byte {
                b'"' => break,
                b'\\' => {
                    let Some(escape) = self.peek() else {
                        return Err("unterminated escape".to_owned());
                    };
                    self.at += 1;
                    let decoded = match escape {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => {
                            let high = self.hex4()?;
                            let code = if (0xD800..0xDC00).contains(&high) {
                                if !self.bytes[self.at..].starts_with(b"\\u") {
                                    return Err("lone high surrogate".to_owned());
                                }
                                self.at += 2;
                                let low = self.hex4()?;
                                if !(0xDC00..0xE000).contains(&low) {
                                    return Err("bad low surrogate".to_owned());
                                }
                                0x10000 + ((high - 0xD800) << 10) + (low - 0xDC00)
                            } else if (0xDC00..0xE000).contains(&high) {
                                return Err("lone low surrogate".to_owned());
                            } else {
                                high
                            };
                            char::from_u32(code).ok_or_else(|| "bad code point".to_owned())?
                        }
                        _ => return Err("bad escape".to_owned()),
                    };
                    let mut buffer = [0u8; 4];
                    out.extend_from_slice(decoded.encode_utf8(&mut buffer).as_bytes());
                }
                0x00..=0x1F => return Err("control character in string".to_owned()),
                _ => out.push(byte),
            }
        }
        String::from_utf8(out).map_err(|_| "invalid UTF-8 in string".to_owned())
    }

    fn digits(&mut self) -> usize {
        let start = self.at;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.at += 1;
        }
        self.at - start
    }

    fn number(&mut self) -> Result<String, String> {
        let start = self.at;
        if self.peek() == Some(b'-') {
            self.at += 1;
        }
        match self.peek() {
            Some(b'0') => self.at += 1,
            Some(b'1'..=b'9') => {
                self.digits();
            }
            _ => return Err("bad number".to_owned()),
        }
        if self.peek() == Some(b'.') {
            self.at += 1;
            if self.digits() == 0 {
                return Err("bad fraction".to_owned());
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.at += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.at += 1;
            }
            if self.digits() == 0 {
                return Err("bad exponent".to_owned());
            }
        }
        Ok(String::from_utf8(self.bytes[start..self.at].to_vec()).expect("ASCII number"))
    }
}

// ---------------------------------------------------------------------------
// The walk.

struct Walker<'t> {
    table: &'t FieldTable,
    out: Walked,
    /// The current record's items by (levels, modality).
    index: HashMap<(Vec<(String, usize)>, String), usize>,
    /// The table's entries by path, in table order.
    by_path: HashMap<&'static str, Vec<&'static Entry>>,
}

/// Where the walk is: the record, its scalar fields, array indices within
/// it, and the line (for occurrence labels).
#[derive(Clone)]
struct Ctx {
    record: Option<usize>,
    scalars: std::rc::Rc<BTreeMap<String, String>>,
    /// Every record-relative member path present in the record.
    present: std::rc::Rc<BTreeSet<String>>,
    /// The record itself, for mirrors that look across it.
    root: std::rc::Rc<J>,
    index: Vec<usize>,
    /// `(array path, position)` of each array the walk is inside.
    levels: Vec<(String, usize)>,
    line: usize,
}

/// A scalar's spelling, as conditions and vocabularies compare it.
fn spelling(value: &J) -> Option<String> {
    match value {
        J::Null => Some("null".to_owned()),
        J::Bool(b) => Some(b.to_string()),
        J::Num(token) => Some(token.clone()),
        J::Str(text) => Some(text.clone()),
        J::Arr(_) | J::Obj(_) => None,
    }
}

fn is_scalar(value: &J) -> bool {
    !matches!(value, J::Arr(_) | J::Obj(_))
}

fn is_empty(value: &J) -> bool {
    match value {
        J::Null => true,
        J::Str(text) => text.is_empty(),
        J::Arr(elements) => elements.is_empty(),
        J::Obj(members) => members.is_empty(),
        J::Bool(_) | J::Num(_) => false,
    }
}

/// Whether a value with nothing to look inside is a leaf.
fn is_leaf(value: &J) -> bool {
    is_scalar(value) || is_empty(value)
}

/// The record's scalar fields by record-relative key path, and the set of
/// every member path present. Arrays are not entered: a condition names one
/// field.
fn scalars(
    value: &J,
    prefix: &str,
    out: &mut BTreeMap<String, String>,
    present: &mut BTreeSet<String>,
) {
    if let J::Obj(members) = value {
        for (name, member) in members {
            let path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}.{name}")
            };
            present.insert(path.clone());
            match member {
                J::Obj(_) => scalars(member, &path, out, present),
                J::Arr(_) => {}
                scalar => {
                    out.insert(path, spelling(scalar).expect("scalar"));
                }
            }
        }
    }
}

fn holds(
    when: &[Cond],
    scalars: &BTreeMap<String, String>,
    present: &BTreeSet<String>,
    parent: Option<&J>,
) -> bool {
    let sibling = |name: &str| match parent {
        Some(J::Obj(members)) => members.get(name).and_then(spelling),
        _ => None,
    };
    when.iter().all(|cond| match cond {
        Cond::Sibling(name, value) => sibling(name).is_some_and(|v| v == *value),
        Cond::SiblingNot(name, value) => sibling(name).is_none_or(|v| v != *value),
        Cond::Is(field, value) => scalars.get(*field).is_some_and(|v| v == value),
        Cond::IsNot(field, value) => scalars.get(*field).is_none_or(|v| v != value),
        Cond::Prefix(field, prefix) => scalars.get(*field).is_some_and(|v| v.starts_with(prefix)),
        Cond::Present(field) => present.contains(*field),
        Cond::Absent(field) => !present.contains(*field),
        Cond::Scalar(field) => scalars.contains_key(*field),
    })
}

/// Whether an entry's class can take this value. A decode takes the JSON
/// type it decodes (a text decode a string, a number decode a number, a
/// vocabulary any scalar, tool JSON and constants anything); a structure
/// and an empty drop take only a leaf; anything else any value.
fn applies(entry: &Entry, value: &J) -> bool {
    match entry.class {
        Class::Fact { decode, .. } => decode_applies(decode, value),
        Class::Annotation(list) => list
            .iter()
            .all(|(_, decode)| decode_applies(*decode, value)),
        // A correlator names one id; an object or array under its path is
        // walked like any other value, so nothing hides inside it.
        Class::Structure(_) | Class::Correlator => is_leaf(value),
        Class::Drop(DropReason::Empty) => is_leaf(value),
        Class::Drop(_) | Class::Open(_) => true,
    }
}

fn decode_applies(decode: Decode, value: &J) -> bool {
    match decode {
        Decode::Json | Decode::Member(_) | Decode::ConstText(_) | Decode::ConstTerm(_) => true,
        Decode::Text
        | Decode::Base64
        | Decode::Base64Url
        | Decode::DataUri
        | Decode::DataUriType
        | Decode::JsonText
        | Decode::MediaType
        | Decode::Timestamp => matches!(value, J::Str(_)),
        Decode::Number | Decode::UnixSeconds => matches!(value, J::Num(_)),
        Decode::Bool => matches!(value, J::Bool(_)),
        Decode::Term(_) | Decode::TextMap(_) => is_scalar(value),
    }
}

fn is_empty_drop(entry: &Entry) -> bool {
    matches!(entry.class, Class::Drop(DropReason::Empty))
}

/// A name safe to print in a path: field names and vocabulary, never
/// content. Anything else prints as a placeholder.
fn printable(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-.:$@".contains(&b))
}

enum Failure {
    Decode,
    UnknownValue,
}

impl<'t> Walker<'t> {
    fn row(&mut self, path: &str, kind: Kind, ctx: &Ctx) {
        self.out.rows.push((
            path.to_owned(),
            kind,
            format!("{}:{:?}", ctx.line, ctx.index),
        ));
    }

    fn document(&mut self, bytes: &[u8], line: usize) {
        let ctx = Ctx {
            record: None,
            scalars: Default::default(),
            present: Default::default(),
            root: std::rc::Rc::new(J::Null),
            index: Vec::new(),
            levels: Vec::new(),
            line,
        };
        match Parser::document(bytes) {
            Ok(value) => self.value(&value, "", None, ctx),
            Err(_) => self.row("<document>", Kind::Decode, &ctx),
        }
    }

    /// The object's shape: the value of each discriminator member it
    /// carries, in table order, joined by `}{`.
    fn discriminator(&self, value: &J) -> Option<String> {
        let J::Obj(members) = value else { return None };
        let shapes: Vec<String> = self
            .table
            .discriminators
            .iter()
            .filter_map(|key| match members.get(*key) {
                Some(J::Str(shape)) if id_like(shape) => Some("<id>".to_owned()),
                Some(J::Str(shape)) if printable(shape) => Some(shape.clone()),
                Some(J::Str(_)) => Some("?".to_owned()),
                _ => None,
            })
            .collect();
        (!shapes.is_empty()).then(|| shapes.join("}{"))
    }

    /// Note the paths whose values unit mirrors compare against.
    fn watch(&mut self) {
        for entry in self.table.entries {
            if let Class::Drop(DropReason::Mirror(Mirror::Unit(paths) | Mirror::Within(paths))) =
                entry.class
            {
                for path in paths {
                    self.out.watched.entry((*path).to_owned()).or_default();
                }
            }
        }
    }

    fn value(&mut self, value: &J, path: &str, parent: Option<&J>, mut ctx: Ctx) {
        if path == self.table.record && ctx.record.is_none() {
            ctx = self.start_record(value, ctx);
        }
        if let Some((hashes, strings)) = self.out.watched.get_mut(path) {
            hashes.insert(value_hash(value));
            if let J::Str(text) = value {
                strings.push(text.clone());
            }
        }
        let shape = self.discriminator(value);
        let shaped = shape.as_ref().map(|shape| format!("{path}{{{shape}}}"));
        if let (Some(record), Some(windows)) = (ctx.record, self.table.windows.requests) {
            if shaped.as_deref() == Some(windows.result.0) || path == windows.result.0 {
                self.out.records[record].results.push(ctx.levels.clone());
            }
        }

        let by_path = &self.by_path;
        let candidates = |at: &str| -> Vec<&'t Entry> {
            let mut entries: Vec<&'t Entry> = by_path
                .get(at)
                .into_iter()
                .flatten()
                .copied()
                .filter(|entry| {
                    entry.path == at
                        && applies(entry, value)
                        && holds(entry.when, &ctx.scalars, &ctx.present, parent)
                })
                .collect();
            if entries.len() > 1 && entries.iter().any(|entry| is_empty_drop(entry)) {
                let valued = entries.iter().any(|entry| {
                    matches!(
                        entry.class,
                        Class::Fact { .. } | Class::Annotation(_) | Class::Structure(_)
                    )
                });
                // An empty drop competes only for an empty value; there it
                // yields to an entry that holds a value and wins over the
                // rest.
                if valued || !is_empty(value) {
                    entries.retain(|entry| !is_empty_drop(entry));
                } else {
                    entries.retain(|entry| is_empty_drop(entry));
                }
            }
            entries
        };
        let mut entries: Vec<&Entry> = shaped.as_deref().map(candidates).unwrap_or_default();
        if entries.is_empty() {
            entries = candidates(path);
        }

        match entries.as_slice() {
            [entry] => {
                let entry = *entry;
                // Rows and origins name the path the entry matched, shape
                // included.
                self.apply(entry, value, entry.path, parent, &ctx);
                return;
            }
            [] => {}
            _ => {
                self.row(path, Kind::Table, &ctx);
                return;
            }
        }
        if is_leaf(value) {
            self.row(path, Kind::Unknown, &ctx);
            return;
        }
        let inner = shaped.unwrap_or_else(|| path.to_owned());
        match value {
            J::Obj(members) => {
                let id_keyed = self.table.id_keyed.contains(&path);
                for (name, member) in members {
                    let child = if id_keyed || id_like(name) {
                        format!("{inner}.<id>")
                    } else if printable(name) {
                        format!("{inner}.{name}")
                    } else {
                        format!("{inner}.<key>")
                    };
                    self.value(member, &child, Some(value), ctx.clone());
                }
            }
            J::Arr(elements) => {
                let child = format!("{inner}[]");
                for (at, element) in elements.iter().enumerate() {
                    let mut ctx = ctx.clone();
                    ctx.index.push(at);
                    // Member order, not spelling: the array's own path.
                    ctx.levels.push((inner.clone(), at));
                    self.value(element, &child, Some(value), ctx);
                }
            }
            _ => unreachable!("leaves are handled above"),
        }
    }

    fn start_record(&mut self, value: &J, mut ctx: Ctx) -> Ctx {
        let mut fields = BTreeMap::new();
        let mut present = BTreeSet::new();
        scalars(value, "", &mut fields, &mut present);
        let field = |name: &str| fields.get(name).filter(|v| *v != "null").cloned();
        let shape = self.discriminator(value);
        let windows = self.table.windows;
        let mut links = RecordLinks {
            shape: shape.clone(),
            ..RecordLinks::default()
        };
        match self.table.order {
            Order::Parents { id, parent } => {
                links.id = field(id);
                links.parent = field(parent);
            }
            Order::File(rules) => {
                links.file = file_kind(&rules, &fields, &present);
            }
        }
        if let Some(compaction) = windows.compaction {
            if shape.as_deref() == Some(compaction.shape) {
                if let (Some(head), Some(tail), Some(anchor)) = (
                    field(compaction.head),
                    field(compaction.tail),
                    field(compaction.anchor),
                ) {
                    links.compaction = Some((head, tail, anchor));
                }
            }
        }
        if let Some(requests) = windows.requests {
            if shape.as_deref() == Some(requests.shape) {
                links.request = field(requests.id);
                let (array, kind, place) = requests.dropped;
                for element in elements(value, array) {
                    if member(element, "type") == Some(kind) {
                        links
                            .dropped
                            .push(member(element, place).unwrap_or_default().to_owned());
                    }
                }
                let (array, kind, count) = requests.cleared;
                for element in elements(value, array) {
                    if member(element, "type") == Some(kind) {
                        let cleared = match element {
                            J::Obj(members) => match members.get(count) {
                                Some(J::Num(token)) => token.parse::<usize>().ok(),
                                _ => None,
                            },
                            _ => None,
                        };
                        links.cleared.push(cleared);
                    }
                }
            }
        }
        let record = self.out.records.len();
        self.index = HashMap::new();
        if let Some(id) = &links.id {
            self.out.ids.entry(id.clone()).or_default().push(record);
        }
        self.out.records.push(links);
        ctx.record = Some(record);
        ctx.scalars = std::rc::Rc::new(fields);
        ctx.present = std::rc::Rc::new(present);
        ctx.root = std::rc::Rc::new(value.clone());
        ctx.index = Vec::new();
        ctx.levels = Vec::new();
        ctx
    }

    fn emit(&mut self, token: Token, path: &str, ctx: &Ctx) {
        let header = self.table.windows.header == Some(path);
        if let (Some(record), false, true) = (ctx.record, token.modality.is_empty(), header) {
            let item = match self.out.records[record].header {
                Some(item) => item,
                None => {
                    self.out.items.push(Item {
                        record,
                        levels: ctx.levels.clone(),
                        modality: token.modality.clone(),
                        tokens: BTreeSet::new(),
                        path: path.to_owned(),
                    });
                    self.out.records[record].header = Some(self.out.items.len() - 1);
                    self.out.items.len() - 1
                }
            };
            self.out.items[item].tokens.insert(token.clone());
        } else if let (Some(record), false) = (ctx.record, token.modality.is_empty()) {
            let links = &mut self.out.records[record];
            let key = (ctx.levels.clone(), token.modality.clone());
            let found = self.index.get(&key).copied();
            let item = match found {
                Some(item) => item,
                None => {
                    self.out.items.push(Item {
                        record,
                        levels: ctx.levels.clone(),
                        modality: token.modality.clone(),
                        tokens: BTreeSet::new(),
                        path: path.to_owned(),
                    });
                    links.items.push(self.out.items.len() - 1);
                    self.index.insert(key, self.out.items.len() - 1);
                    self.out.items.len() - 1
                }
            };
            let tokens = &mut self.out.items[item].tokens;
            if tokens.first().is_none_or(|first| token < *first) {
                self.out.items[item].path = path.to_owned();
            }
            self.out.items[item].tokens.insert(token.clone());
        }
        self.out.tokens.entry(token).or_default().push(Origin {
            path: path.to_owned(),
            record: ctx.record,
            index: ctx.index.clone(),
        });
    }

    fn apply(&mut self, entry: &Entry, value: &J, path: &str, parent: Option<&J>, ctx: &Ctx) {
        match entry.class {
            Class::Fact {
                modality,
                direction,
                attribute,
                decode,
                with,
            } => match decode_values(decode, value) {
                Ok(values) => {
                    let found = !values.is_empty();
                    for value in values {
                        let token = Token {
                            modality: modality.to_owned(),
                            direction: direction.to_owned(),
                            attribute: attribute.to_owned(),
                            value,
                        };
                        self.emit(token, path, ctx);
                    }
                    // A fixed companion belongs to a fact only when the
                    // value made one.
                    for (attribute, decode) in with.iter().filter(|_| found) {
                        match decode_values(*decode, value) {
                            Ok(values) => {
                                for value in values {
                                    let token = Token {
                                        modality: modality.to_owned(),
                                        direction: direction.to_owned(),
                                        attribute: (*attribute).to_owned(),
                                        value,
                                    };
                                    self.emit(token, path, ctx);
                                }
                            }
                            Err(failure) => self.fail(failure, path, ctx),
                        }
                    }
                }
                Err(failure) => self.fail(failure, path, ctx),
            },
            Class::Annotation(list) => {
                for (attribute, decode) in list {
                    match decode_values(*decode, value) {
                        Ok(values) => {
                            for value in values {
                                let token = Token {
                                    modality: String::new(),
                                    direction: String::new(),
                                    attribute: (*attribute).to_owned(),
                                    value,
                                };
                                self.emit(token, path, ctx);
                            }
                        }
                        Err(failure) => self.fail(failure, path, ctx),
                    }
                }
            }
            Class::Structure(structure) => match structure {
                Structure::Discriminator => {
                    if !matches!(value, J::Str(_)) {
                        self.row(path, Kind::Decode, ctx);
                    }
                }
                Structure::Vocabulary(terms) => {
                    if !spelling(value).is_some_and(|v| terms.contains(&v.as_str())) {
                        self.row(path, Kind::UnknownValue, ctx);
                    }
                }
                Structure::Order => {
                    if !matches!(value, J::Str(_) | J::Null) {
                        self.row(path, Kind::Decode, ctx);
                    }
                }
                Structure::Shape(_) => {}
            },
            Class::Correlator => {}
            Class::Drop(reason) => match reason {
                DropReason::Machinery(_) | DropReason::ProviderId => {}
                DropReason::Empty => {
                    if !is_empty(value) {
                        self.row(path, Kind::Empty, ctx);
                    }
                }
                DropReason::Mirror(Mirror::Sibling(name)) => {
                    let mirrored = match parent {
                        Some(J::Obj(members)) => members.get(name),
                        _ => None,
                    };
                    if !mirrored.is_some_and(|mirrored| same(mirrored, value)) {
                        self.row(path, Kind::Mirror, ctx);
                    }
                }
                DropReason::Mirror(Mirror::Keyed { array, key, member }) => {
                    let J::Obj(mirrors) = value else {
                        self.row(path, Kind::Mirror, ctx);
                        return;
                    };
                    let mut elements: &[J] = &[];
                    let mut at: Option<&J> = Some(&ctx.root);
                    for step in array.split('.') {
                        at = match at {
                            Some(J::Obj(members)) => members.get(step),
                            _ => None,
                        };
                    }
                    if let Some(J::Arr(found)) = at {
                        elements = found;
                    }
                    for (name, mirror) in mirrors {
                        let original = elements.iter().find_map(|element| match element {
                            J::Obj(members)
                                if matches!(members.get(key), Some(J::Str(id)) if id == name) =>
                            {
                                members.get(member)
                            }
                            _ => None,
                        });
                        if !original.is_some_and(|original| same(original, mirror)) {
                            self.row(path, Kind::Mirror, ctx);
                        }
                    }
                }
                DropReason::Mirror(mirror @ (Mirror::Unit(_) | Mirror::Within(_))) => {
                    let text = match value {
                        J::Str(text) => Some(text.clone()),
                        _ => None,
                    };
                    if matches!(mirror, Mirror::Within(_)) && text.is_none() {
                        self.row(path, Kind::Mirror, ctx);
                        return;
                    }
                    let label = format!("{}:{:?}", ctx.line, ctx.index);
                    self.out.pending.push((
                        path.to_owned(),
                        mirror,
                        value_hash(value),
                        text,
                        label,
                    ));
                }
                DropReason::Mirror(Mirror::Field(field)) => {
                    if !field_at(&ctx.root, field).is_some_and(|original| same(original, value)) {
                        self.row(path, Kind::Mirror, ctx);
                    }
                }
                DropReason::Mirror(Mirror::Children) => {
                    let ids: Option<Vec<String>> = match value {
                        J::Arr(elements) => elements
                            .iter()
                            .map(|element| match element {
                                J::Str(id) => Some(id.clone()),
                                _ => None,
                            })
                            .collect(),
                        _ => None,
                    };
                    match (ids, ctx.record) {
                        (Some(ids), Some(record)) => {
                            self.out.records[record].children = Some((path.to_owned(), ids));
                        }
                        _ => self.row(path, Kind::Decode, ctx),
                    }
                }
            },
            Class::Open(_) => self.row(path, Kind::Open, ctx),
        }
    }

    fn fail(&mut self, failure: Failure, path: &str, ctx: &Ctx) {
        let kind = match failure {
            Failure::Decode => Kind::Decode,
            Failure::UnknownValue => Kind::UnknownValue,
        };
        self.row(path, kind, ctx);
    }

    /// Check the children mirrors against the parent links, put each
    /// record's items in record order, and check that a repeated record
    /// holds what the first one does.
    fn finish(&mut self) {
        let mut rows = Vec::new();
        for record in 0..self.out.records.len() {
            let mut items = std::mem::take(&mut self.out.records[record].items);
            items.sort_by(|a, b| self.out.items[*a].levels.cmp(&self.out.items[*b].levels));
            self.out.records[record].items = items;
        }
        let windows = self.table.windows;
        let mut latest = None;
        for record in 0..self.out.records.len() {
            self.out.prefixes.push(latest);
            let links = &self.out.records[record];
            let prefix = links
                .shape
                .as_deref()
                .is_some_and(|shape| windows.prefix.contains(&shape));
            if prefix && !self.out.is_repeat(record) {
                latest = Some(record);
            }
            if let (Some((_, _, anchor)), false) = (&links.compaction, self.out.is_repeat(record)) {
                self.out.anchors.entry(anchor.clone()).or_insert(record);
            }
        }
        for record in 0..self.out.records.len() {
            if !self.out.is_repeat(record) {
                continue;
            }
            let first = self.out.records[record]
                .id
                .as_deref()
                .and_then(|id| self.out.first(id))
                .expect("a repeat has a first");
            let held = |at: usize| -> Vec<&BTreeSet<Token>> {
                self.out.records[at]
                    .items
                    .iter()
                    .map(|&item| &self.out.items[item].tokens)
                    .collect()
            };
            if held(first) != held(record) {
                let id = match self.table.order {
                    Order::Parents { id, .. } => id,
                    Order::File(_) => "id",
                };
                let shape = self.out.records[record].shape.as_deref().unwrap_or("?");
                rows.push((
                    format!("{{{shape}}}.{id}"),
                    Kind::Count,
                    format!("record {record}"),
                ));
            }
        }
        for (at, record) in self.out.records.iter().enumerate() {
            let Some((path, listed)) = &record.children else {
                continue;
            };
            let listed: BTreeSet<&str> = listed.iter().map(String::as_str).collect();
            let linked: BTreeSet<&str> = self
                .out
                .records
                .iter()
                .filter(|child| child.parent.is_some() && child.parent == record.id)
                .filter_map(|child| child.id.as_deref())
                .collect();
            if listed != linked {
                rows.push((path.clone(), Kind::Mirror, format!("record {at}")));
            }
        }
        if let Order::File(_) = self.table.order {
            self.trails();
        }
        let mut last = None;
        for record in 0..self.out.records.len() {
            self.out.file_parent.push(last);
            if !self.out.records[record].items.is_empty() {
                last = Some(record);
            }
        }
        let mut memo: HashMap<([u8; 32], usize), bool> = HashMap::new();
        for (path, mirror, hash, text, label) in std::mem::take(&mut self.out.pending) {
            let key = match mirror {
                Mirror::Unit(paths) | Mirror::Within(paths) => (hash, paths.as_ptr() as usize),
                _ => (hash, 0),
            };
            if let Some(&held) = memo.get(&key) {
                if !held {
                    rows.push((path, Kind::Mirror, label));
                }
                continue;
            }
            let held = match mirror {
                Mirror::Unit(paths) => paths.iter().any(|at| {
                    self.out
                        .watched
                        .get(*at)
                        .is_some_and(|(hashes, _)| hashes.contains(&hash))
                }),
                Mirror::Within(paths) => {
                    paths.iter().any(|at| {
                        self.out
                            .watched
                            .get(*at)
                            .is_some_and(|(hashes, _)| hashes.contains(&hash))
                    }) || text.as_deref().is_some_and(|text| {
                        paths.iter().any(|at| {
                            self.out.watched.get(*at).is_some_and(|(_, strings)| {
                                strings.iter().any(|haystack| haystack.contains(text))
                            })
                        })
                    })
                }
                _ => true,
            };
            memo.insert(key, held);
            if !held {
                rows.push((path, Kind::Mirror, label));
            }
        }
        self.out.rows.extend(rows);
    }
}

impl Walker<'_> {
    /// [`Order::File`]: one pass over the records, leaving for each what
    /// precedes its items.
    fn trails(&mut self) {
        let out = &mut self.out;
        let mut header: Vec<usize> = Vec::new();
        let mut prefix: Vec<usize> = Vec::new();
        // The line's places in the arena, in order.
        let mut line: Vec<usize> = Vec::new();
        let mut turns: Vec<usize> = Vec::new();
        let mut broken = false;
        for at in 0..out.records.len() {
            let own = out.ordered(at);
            let opening = |header: &[usize], prefix: &[usize]| -> Vec<usize> {
                header.iter().chain(prefix).copied().collect()
            };
            let kind = out.records[at].file;
            let trail = match kind {
                FileKind::Header => FileTrail::default(),
                FileKind::Prefix => FileTrail {
                    opening: header.clone(),
                    ..FileTrail::default()
                },
                FileKind::Replace => FileTrail {
                    opening: opening(&header, &prefix),
                    ..FileTrail::default()
                },
                _ => FileTrail {
                    opening: opening(&header, &prefix),
                    tail: line.last().copied(),
                    broken,
                },
            };
            out.file_trails.push(trail);
            let mut push = |line: &mut Vec<usize>, item: usize| {
                let previous = line.last().copied();
                out.file_arena.push((item, previous));
                line.push(out.file_arena.len() - 1);
            };
            match kind {
                FileKind::Header => header = own,
                FileKind::Prefix => prefix = own,
                FileKind::Replace => {
                    line.clear();
                    for item in own {
                        push(&mut line, item);
                    }
                    turns.clear();
                    broken = false;
                }
                FileKind::Turn => turns.push(line.len()),
                FileKind::Rollback(count) => match count {
                    Some(0) => {}
                    Some(count) if count <= turns.len() && !broken => {
                        let keep = turns.len() - count;
                        line.truncate(turns[keep]);
                        turns.truncate(keep);
                    }
                    _ => broken = true,
                },
                FileKind::Content => {
                    for item in own {
                        push(&mut line, item);
                    }
                }
            }
        }
    }
}

/// A record's kind under [`Order::File`]: the first rule whose conditions
/// all hold.
fn file_kind(
    rules: &FileWindows,
    fields: &BTreeMap<String, String>,
    present: &BTreeSet<String>,
) -> FileKind {
    let is = |when: &[Cond]| !when.is_empty() && holds(when, fields, present, None);
    if is(rules.header) {
        FileKind::Header
    } else if is(rules.prefix) {
        FileKind::Prefix
    } else if is(rules.replace) {
        FileKind::Replace
    } else if is(rules.turn) {
        FileKind::Turn
    } else if is(rules.rollback) {
        FileKind::Rollback(
            fields
                .get(rules.rollback_count)
                .and_then(|count| count.parse::<usize>().ok()),
        )
    } else {
        FileKind::Content
    }
}

/// A hash of a JSON value, members in name order, numbers by token.
fn value_hash(value: &J) -> [u8; 32] {
    fn feed(value: &J, hasher: &mut blake3::Hasher) {
        match value {
            J::Null => {
                hasher.update(b"n");
            }
            J::Bool(b) => {
                hasher.update(if *b { b"t" } else { b"f" });
            }
            J::Num(token) => {
                hasher.update(b"#");
                hasher.update(&(token.len() as u64).to_le_bytes());
                hasher.update(token.as_bytes());
            }
            J::Str(text) => {
                hasher.update(b"s");
                hasher.update(&(text.len() as u64).to_le_bytes());
                hasher.update(text.as_bytes());
            }
            J::Arr(elements) => {
                hasher.update(b"[");
                hasher.update(&(elements.len() as u64).to_le_bytes());
                for element in elements {
                    feed(element, hasher);
                }
            }
            J::Obj(members) => {
                hasher.update(b"{");
                hasher.update(&(members.len() as u64).to_le_bytes());
                for (name, member) in members {
                    hasher.update(&(name.len() as u64).to_le_bytes());
                    hasher.update(name.as_bytes());
                    feed(member, hasher);
                }
            }
        }
    }
    let mut hasher = blake3::Hasher::new();
    feed(value, &mut hasher);
    *hasher.finalize().as_bytes()
}

/// The value at a record-relative member path.
fn field_at<'j>(root: &'j J, path: &str) -> Option<&'j J> {
    let mut at = Some(root);
    for step in path.split('.') {
        at = match at {
            Some(J::Obj(members)) => members.get(step),
            _ => None,
        };
    }
    at
}

/// The elements of the array at a record-relative path (none when there is
/// no array there).
fn elements<'j>(root: &'j J, path: &str) -> &'j [J] {
    let mut at = Some(root);
    for step in path.split('.') {
        at = match at {
            Some(J::Obj(members)) => members.get(step),
            _ => None,
        };
    }
    match at {
        Some(J::Arr(elements)) => elements,
        _ => &[],
    }
}

/// A string member of an object.
fn member<'j>(value: &'j J, name: &str) -> Option<&'j str> {
    match value {
        J::Obj(members) => match members.get(name) {
            Some(J::Str(text)) => Some(text),
            _ => None,
        },
        _ => None,
    }
}

/// Order-free equality of two JSON values; numbers by token.
fn same(a: &J, b: &J) -> bool {
    match (a, b) {
        (J::Null, J::Null) => true,
        (J::Bool(x), J::Bool(y)) => x == y,
        (J::Num(x), J::Num(y)) | (J::Str(x), J::Str(y)) => x == y,
        (J::Arr(x), J::Arr(y)) => x.len() == y.len() && x.iter().zip(y).all(|(x, y)| same(x, y)),
        (J::Obj(x), J::Obj(y)) => {
            x.len() == y.len()
                && x.iter()
                    .zip(y)
                    .all(|((kx, vx), (ky, vy))| kx == ky && same(vx, vy))
        }
        _ => false,
    }
}

fn decode_values(decode: Decode, value: &J) -> Result<Vec<Value>, Failure> {
    let one = |value: Value| Ok(vec![value]);
    match (decode, value) {
        // `""` is never stored: it is no value.
        (Decode::Text, J::Str(text)) if text.is_empty() => Ok(Vec::new()),
        (Decode::Text, J::Str(text)) => one(Value::Text(text.clone())),
        (Decode::ConstText(text), _) => one(Value::Text(text.to_owned())),
        (Decode::ConstTerm(term), _) => one(Value::Term(term.to_owned())),
        (Decode::TextMap(map), value) => {
            let spelled = spelling(value).ok_or(Failure::Decode)?;
            match map.iter().find(|(source, _)| *source == spelled) {
                Some((_, text)) => one(Value::Text((*text).to_owned())),
                None => Err(Failure::UnknownValue),
            }
        }
        (Decode::Base64, J::Str(text)) => {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(text)
                .map_err(|_| Failure::Decode)?;
            one(Value::Bytes(blake3::hash(&bytes).to_hex().to_string()))
        }
        (Decode::Base64Url, J::Str(text)) => {
            let engine = base64::engine::GeneralPurpose::new(
                &base64::alphabet::URL_SAFE,
                base64::engine::GeneralPurposeConfig::new()
                    .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent),
            );
            let bytes = engine.decode(text).map_err(|_| Failure::Decode)?;
            one(Value::Bytes(blake3::hash(&bytes).to_hex().to_string()))
        }
        (Decode::DataUri, J::Str(text)) => {
            let (_, data) = data_uri(text).ok_or(Failure::Decode)?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data)
                .map_err(|_| Failure::Decode)?;
            one(Value::Bytes(blake3::hash(&bytes).to_hex().to_string()))
        }
        (Decode::DataUriType, J::Str(text)) => {
            let (media_type, _) = data_uri(text).ok_or(Failure::Decode)?;
            one(Value::Text(
                canonical_media_type(media_type).ok_or(Failure::Decode)?,
            ))
        }
        (Decode::JsonText, J::Str(text)) => {
            let parsed = Parser::document(text.as_bytes()).map_err(|_| Failure::Decode)?;
            let mut leaves = Vec::new();
            json_leaves(&parsed, &mut String::new(), &mut leaves);
            Ok(leaves)
        }
        (Decode::Number, J::Num(token)) => one(Value::Number(token.clone())),
        (Decode::Bool, J::Bool(b)) => one(Value::Bool(*b)),
        (Decode::Json, value) => {
            let mut leaves = Vec::new();
            json_leaves(value, &mut String::new(), &mut leaves);
            Ok(leaves)
        }
        (Decode::Member(pointer), value) => {
            let mut leaves = Vec::new();
            json_leaves(value, &mut pointer.to_owned(), &mut leaves);
            Ok(leaves)
        }
        (Decode::MediaType, J::Str(text)) => one(Value::Text(
            canonical_media_type(text).ok_or(Failure::Decode)?,
        )),
        (Decode::Timestamp, J::Str(text)) => {
            let epoch = Epoch::from_str(text.trim()).map_err(|_| Failure::Decode)?;
            one(Value::Time(epoch.to_tai_duration().total_nanoseconds()))
        }
        (Decode::UnixSeconds, J::Num(token)) => {
            let nanos = exact_nanoseconds(token).ok_or(Failure::Decode)?;
            let epoch = Epoch::from_unix_duration(Duration::from_total_nanoseconds(nanos));
            one(Value::Time(epoch.to_tai_duration().total_nanoseconds()))
        }
        (Decode::Term(map), value) => {
            let spelled = spelling(value).ok_or(Failure::Decode)?;
            match map.iter().find(|(source, _)| *source == spelled) {
                Some((_, term)) => one(Value::Term((*term).to_owned())),
                None => Err(Failure::UnknownValue),
            }
        }
        _ => Err(Failure::Decode),
    }
}

/// A base64 `data:` URI's media type and data.
fn data_uri(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix("data:")?;
    let (head, data) = rest.split_once(',')?;
    let media_type = head.strip_suffix(";base64")?;
    Some((media_type, data))
}

/// Every leaf of a JSON value under its JSON Pointer.
fn json_leaves(value: &J, pointer: &mut String, out: &mut Vec<Value>) {
    let leaf = match value {
        J::Null => JsonLeaf::Null,
        J::Bool(b) => JsonLeaf::Bool(*b),
        J::Num(token) => JsonLeaf::Number(token.clone()),
        J::Str(text) => JsonLeaf::String(text.clone()),
        J::Arr(elements) if elements.is_empty() => JsonLeaf::EmptyArray,
        J::Obj(members) if members.is_empty() => JsonLeaf::EmptyObject,
        J::Arr(elements) => {
            for (at, element) in elements.iter().enumerate() {
                let len = pointer.len();
                pointer.push('/');
                pointer.push_str(&at.to_string());
                json_leaves(element, pointer, out);
                pointer.truncate(len);
            }
            return;
        }
        J::Obj(members) => {
            for (name, member) in members {
                let len = pointer.len();
                pointer.push('/');
                pointer.push_str(&name.replace('~', "~0").replace('/', "~1"));
                json_leaves(member, pointer, out);
                pointer.truncate(len);
            }
            return;
        }
    };
    out.push(Value::Json(pointer.clone(), leaf));
}

fn is_tchar(c: char) -> bool {
    c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c)
}

fn is_token(text: &str) -> bool {
    !text.is_empty() && text.chars().all(is_tchar)
}

/// The canonical spelling of a media type: type, subtype and parameter
/// names lowercased, parameters sorted by name, values exact, a value
/// quoted only when it is not a token. A malformed spelling or a repeated
/// parameter is `None`.
fn canonical_media_type(text: &str) -> Option<String> {
    let mut rest = text.trim_matches(|c| c == ' ' || c == '\t');
    let end = rest.find(';').unwrap_or(rest.len());
    let essence = rest[..end].trim_end_matches([' ', '\t']);
    rest = &rest[end..];
    let (kind, sub) = essence.split_once('/')?;
    if !is_token(kind) || !is_token(sub) {
        return None;
    }
    let mut parameters: BTreeMap<String, String> = BTreeMap::new();
    while let Some(after) = rest.strip_prefix(';') {
        let after = after.trim_start_matches([' ', '\t']);
        let eq = after.find('=')?;
        let name = &after[..eq];
        if !is_token(name) {
            return None;
        }
        let mut value_text = &after[eq + 1..];
        let value = if let Some(quoted) = value_text.strip_prefix('"') {
            let mut value = String::new();
            let mut chars = quoted.char_indices();
            let mut close = None;
            while let Some((at, c)) = chars.next() {
                match c {
                    '"' => {
                        close = Some(at);
                        break;
                    }
                    '\\' => value.push(chars.next()?.1),
                    c => value.push(c),
                }
            }
            value_text = &quoted[close? + 1..];
            value
        } else {
            let end = value_text.find(';').unwrap_or(value_text.len());
            let value = value_text[..end].trim_end_matches([' ', '\t']);
            if !is_token(value) {
                return None;
            }
            value_text = &value_text[end..];
            value.to_owned()
        };
        rest = value_text.trim_start_matches([' ', '\t']);
        if parameters
            .insert(name.to_ascii_lowercase(), value)
            .is_some()
        {
            return None;
        }
    }
    if !rest.is_empty() {
        return None;
    }
    let mut out = format!("{}/{}", kind.to_ascii_lowercase(), sub.to_ascii_lowercase());
    for (name, value) in parameters {
        out.push(';');
        out.push_str(&name);
        out.push('=');
        if is_token(&value) {
            out.push_str(&value);
        } else {
            out.push('"');
            for c in value.chars() {
                if c == '"' || c == '\\' {
                    out.push('\\');
                }
                out.push(c);
            }
            out.push('"');
        }
    }
    Some(out)
}

/// An exact decimal number of seconds as nanoseconds, or `None` when it is
/// not a whole number of nanoseconds or does not fit.
fn exact_nanoseconds(token: &str) -> Option<i128> {
    let (negative, unsigned) = match token.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, token),
    };
    let (mantissa, exponent) = match unsigned.find(['e', 'E']) {
        Some(at) => (&unsigned[..at], unsigned[at + 1..].parse::<i32>().ok()?),
        None => (unsigned, 0),
    };
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits = format!("{whole}{fraction}");
    // value = digits * 10^(exponent - fraction.len()); nanoseconds add 9.
    let shift = exponent - fraction.len() as i32 + 9;
    let mut digits = digits.trim_start_matches('0').to_owned();
    if digits.is_empty() {
        return Some(0);
    }
    if shift >= 0 {
        digits.push_str(&"0".repeat(shift as usize));
    } else {
        let cut = digits.len().checked_sub((-shift) as usize)?;
        if !digits[cut..].bytes().all(|b| b == b'0') {
            return None;
        }
        digits.truncate(cut);
        if digits.is_empty() {
            return Some(0);
        }
    }
    let magnitude: i128 = digits.parse().ok()?;
    Some(if negative { -magnitude } else { magnitude })
}

#[cfg(test)]
pub(super) mod probe {
    //! Test access to the walker's internals.
    pub fn canonical_media_type(text: &str) -> Option<String> {
        super::canonical_media_type(text)
    }
    pub fn exact_nanoseconds(token: &str) -> Option<i128> {
        super::exact_nanoseconds(token)
    }
}
