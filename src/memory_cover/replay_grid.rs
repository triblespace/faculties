//! EXPERIMENTAL replay instrument for the grid's sealed head (lane
//! cover-underfill-20261115). Not a recollection rule.
//!
//! The replay frame is carried over from the prototype lane's
//! `replay_tiled` (faculties cover-sealed-20261108 42d862a2,
//! `memory_cover/sealed.rs`): replay points every `step_units * 2^14 s`
//! counted back from a fixed point, a memory visible at a point once its
//! first `created_at` (range end when it has none) is at or before it,
//! exact-span classes, class charge = the largest member's rendered
//! characters. Two things are stricter than there: classes are formed per
//! point from the members visible at that point (so a class's charge and its
//! rendered representative are what existed then), and a respanned memory
//! stands aside only from the point its respan became visible. Selection is
//! b110455f's [`select_grid_recollection_cut`] through
//! [`select_grid_recollection_cut_with`]; nothing of the prototype's
//! selection is used.
//!
//! The metrics are about the KV-cache backend that saves ONE exact token
//! prefix ending at the seal and reuses it only when the new prompt starts
//! with ALL of it. Entries are compared by a digest of their exact rendered
//! text (`"\n{range}\n{body}\n"`, as `render_cover_report` writes it), so
//! "starts with" is an entry-sequence prefix test over exact bytes.

use super::grid::{select_grid_recollection_cut_with, LevelOneRule};
use super::*;

/// One memory as the replay sees it.
#[derive(Clone, Debug)]
pub struct ReplayMember {
    pub span: (i128, i128, Id),
    /// First `created_at`, or the range end when there is none.
    pub observed: i128,
    pub observed_fallback: bool,
    /// From this point on a respan with identical content stands for it.
    pub hidden_from: Option<i128>,
    /// Exact rendered characters of its cover entry.
    pub chars: usize,
    /// Digest of its exact rendered cover entry text.
    pub digest: [u8; 32],
}

/// One rendered cover entry: digest of its exact text and its characters.
pub type Entry = ([u8; 32], usize);

/// The rendered entry text, exactly as `render_cover_report` writes one
/// selected recollection (no raw mode).
pub fn entry_text(start: i128, end: i128, body: Option<&str>) -> String {
    let range = format_time_range(key_to_epoch(start), key_to_epoch(end));
    match body {
        Some(body) => format!("\n{range}\n{}\n", body.trim_end()),
        None => format!("\n{range}\n"),
    }
}

/// Every memory the facts hold, with what the replay needs to place it in
/// time and render it. Returns the members and the number left out because
/// their summary bytes are not readable here (as the render skips them).
pub fn replay_members<B: BlobStoreGet, P: TriblePattern>(
    space: &P,
    ws: &B,
) -> Result<(Vec<ReplayMember>, usize)> {
    // `collect_chunk_spans` without its respan filter: the filter is applied
    // per point below.
    let mut spans: Vec<_> = find!(
        (id: Id, start: Inline<NsTAIInterval>, end: Inline<NsTAIInterval>),
        pattern!(space, [{
            ?id @ metadata::tag: &KIND_CHUNK_ID,
            ctx::start_at: ?start,
            ctx::end_at: ?end,
        }])
    )
    .map(|(id, start, end)| (interval_key(start), interval_key(end), id))
    .filter(|(start, end, _)| start <= end)
    .collect();
    spans.sort_unstable();
    spans.dedup();

    let mut first_by_id: HashMap<Id, i128> = HashMap::new();
    for (id, observed) in find!(
        (id: Id, observed: Inline<NsTAIInterval>),
        pattern!(space, [{ ?id @ metadata::created_at: ?observed }])
    ) {
        let observed = interval_key(observed);
        first_by_id
            .entry(id)
            .and_modify(|first| *first = (*first).min(observed))
            .or_insert(observed);
    }

    let content_of = |id: Id| -> Option<[u8; 32]> {
        chunk_summary_handle(space, id)
            .map(|h| h.raw)
            .or_else(|| chunk_image_handle(space, id).map(|h| h.raw))
    };
    // older -> the earliest point a respan of it with identical content is
    // visible (the newer chunk's own observation).
    let mut hidden_from: HashMap<Id, i128> = HashMap::new();
    let ends: HashMap<Id, i128> = spans.iter().map(|s| (s.2, s.1)).collect();
    for (newer, older) in find!(
        (newer: Id, older: Id),
        pattern!(space, [{
            ?newer @ metadata::tag: &KIND_CHUNK_ID,
            metadata::supersedes: ?older,
        }])
    ) {
        let content = content_of(newer);
        if content.is_none() || content != content_of(older) {
            continue;
        }
        let at = first_by_id
            .get(&newer)
            .copied()
            .or_else(|| ends.get(&newer).copied())
            .unwrap_or(i128::MIN);
        hidden_from
            .entry(older)
            .and_modify(|first| *first = (*first).min(at))
            .or_insert(at);
    }

    let mut members = Vec::with_capacity(spans.len());
    let mut unreadable = 0usize;
    for &(start, end, id) in &spans {
        let body: Option<String> = match chunk_summary_handle(space, id) {
            Some(handle) => match ws.get::<View<str>, UTF8String>(handle) {
                Ok(summary) => Some(summary.to_string()),
                Err(_) => {
                    unreadable += 1;
                    continue;
                }
            },
            None if chunk_image_handle(space, id).is_some() => Some(format!(
                "[image memory @ {}]",
                format_time_range(key_to_epoch(start), key_to_epoch(end))
            )),
            None => None,
        };
        let text = entry_text(start, end, body.as_deref());
        let (observed, observed_fallback) = match first_by_id.get(&id) {
            Some(&at) => (at, false),
            None => (end, true),
        };
        members.push(ReplayMember {
            span: (start, end, id),
            observed,
            observed_fallback,
            hidden_from: hidden_from.get(&id).copied(),
            chars: text.chars().count(),
            digest: *blake3::hash(text.as_bytes()).as_bytes(),
        });
    }
    Ok((members, unreadable))
}

/// Digest over the sorted input set actually used: every member's id, span,
/// observation, respan visibility, characters and text digest.
pub fn input_digest(members: &[ReplayMember]) -> [u8; 32] {
    let mut sorted: Vec<&ReplayMember> = members.iter().collect();
    sorted.sort_by(|a, b| a.span.cmp(&b.span));
    let mut hasher = blake3::Hasher::new();
    for m in sorted {
        hasher.update(&m.span.2.raw());
        hasher.update(&m.span.0.to_be_bytes());
        hasher.update(&m.span.1.to_be_bytes());
        hasher.update(&m.observed.to_be_bytes());
        hasher.update(&m.hidden_from.unwrap_or(i128::MAX).to_be_bytes());
        hasher.update(&(m.chars as u64).to_be_bytes());
        hasher.update(&m.digest);
    }
    *hasher.finalize().as_bytes()
}

/// The classes visible at `point`: spans, charges, and the rendered entry of
/// each class's representative (its least id, as the plain render picks).
pub fn classes_at(
    members: &[ReplayMember],
    point: i128,
) -> (Vec<(i128, i128, Id)>, Vec<usize>, Vec<Entry>) {
    let mut spans: Vec<(i128, i128, Id)> = Vec::new();
    let mut costs: Vec<usize> = Vec::new();
    let mut entries: Vec<Entry> = Vec::new();
    let mut visible: Vec<&ReplayMember> = members
        .iter()
        .filter(|m| m.observed <= point && m.hidden_from.is_none_or(|at| at > point))
        .collect();
    visible.sort_by(|a, b| a.span.cmp(&b.span));
    for m in visible {
        let (start, end, id) = m.span;
        if let Some(last) = spans.last() {
            if last.0 == start && last.1 == end {
                let k = costs.len() - 1;
                costs[k] = costs[k].max(m.chars);
                continue;
            }
        }
        spans.push((start, end, id));
        costs.push(m.chars);
        entries.push((m.digest, m.chars));
    }
    (spans, costs, entries)
}

/// What one candidate rendered at one point.
#[derive(Clone, Debug)]
pub struct StepCover {
    pub point: i128,
    /// Class charge selected (`RecollectionCut::used`).
    pub used: usize,
    /// Level-1 allowance deliberately left unused (UNDERFILL only).
    pub withheld: usize,
    pub entries: Vec<Entry>,
    pub head: usize,
}

impl StepCover {
    pub fn chars(&self) -> usize {
        self.entries.iter().map(|e| e.1).sum()
    }
    pub fn head_chars(&self) -> usize {
        self.entries[..self.head].iter().map(|e| e.1).sum()
    }
}

/// The saved-prefix backend over a sequence of renders.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    /// Saves the head at a (re)seal only; reuses it while every new prompt
    /// starts with it.
    RestoreOnly,
    /// HYPOTHETICAL: may also extend the saved head to a longer new head of
    /// which it is a strict prefix, charging the appended segment once.
    Extend,
}

/// One evaluation of the backend.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BackendStep {
    /// The saved prefix was not a prefix of the new prompt.
    pub reseal: bool,
    /// The saved prefix was extended (Extend only).
    pub extended: bool,
    /// Characters recomputed: the whole prompt on a reseal or the first
    /// render, otherwise everything after the saved prefix.
    pub recomputed: usize,
    /// Characters reused from the saved prefix.
    pub reused: usize,
}

/// Run the backend over `covers` in order. The first render seals and
/// recomputes everything (reported separately from reseals).
pub fn run_backend(covers: &[&StepCover], backend: Backend) -> Vec<BackendStep> {
    let mut saved: Option<Vec<Entry>> = None;
    let mut out = Vec::with_capacity(covers.len());
    for cover in covers {
        let head = &cover.entries[..cover.head];
        let total = cover.chars();
        let step = match &saved {
            None => {
                saved = Some(head.to_vec());
                BackendStep {
                    reseal: false,
                    extended: false,
                    recomputed: total,
                    reused: 0,
                }
            }
            Some(prefix) if cover.entries.starts_with(prefix) => {
                let reused: usize = prefix.iter().map(|e| e.1).sum();
                let mut extended = false;
                if backend == Backend::Extend
                    && head.len() > prefix.len()
                    && head.starts_with(prefix)
                {
                    extended = true;
                }
                if extended {
                    saved = Some(head.to_vec());
                }
                BackendStep {
                    reseal: false,
                    extended,
                    recomputed: total - reused,
                    reused,
                }
            }
            Some(_) => {
                saved = Some(head.to_vec());
                BackendStep {
                    reseal: true,
                    extended: false,
                    recomputed: total,
                    reused: 0,
                }
            }
        };
        out.push(step);
    }
    out
}

/// Render every point under one rule.
pub fn replay_points(
    members: &[ReplayMember],
    points: &[i128],
    budget: usize,
    rules: &[LevelOneRule],
) -> Vec<Vec<StepCover>> {
    let mut out: Vec<Vec<StepCover>> = vec![Vec::with_capacity(points.len()); rules.len()];
    for &point in points {
        let (spans, costs, entries) = classes_at(members, point);
        if spans.is_empty() {
            continue;
        }
        let eligible = vec![true; spans.len()];
        for (r, &rule) in rules.iter().enumerate() {
            let (cut, detail) =
                select_grid_recollection_cut_with(&spans, &costs, &eligible, budget, rule);
            out[r].push(StepCover {
                point,
                used: cut.used,
                withheld: detail.withheld,
                entries: cut.cover.iter().map(|&i| entries[i]).collect(),
                head: cut.head,
            });
        }
    }
    out
}
