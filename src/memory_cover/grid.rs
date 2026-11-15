//! The clock-closed geometric grid over lived time ([`Selector::Grid`]).
//!
//! Settled with JP on 2026-11-08 and ported from the prototype's
//! "tiled 2^16 grid, equal shares, open from free room, oldest cut"
//! configuration (faculties cover-sealed-20261108 42d862a2,
//! `memory_cover/sealed.rs`), and nothing else of it.
//!
//! [`Selector::Grid`]: super::Selector::Grid

use super::{recollection_walk, Id, RecollectionCut};

/// The grid unit: 2^16 s, about 18.2 hours. Level `l` tiles are `4^l` units
/// wide on an absolute grid (multiples of the unit since the TAI epoch), so a
/// boundary is a pure function of lived time, never of characters or of when
/// a memory was written.
pub const GRID_UNIT_NS: i128 = (1i128 << 16) * 1_000_000_000;

/// The open unit's reserve: this fraction of the budget is kept off the closed
/// levels, which share the rest equally. The open unit also takes whatever the
/// closed levels leave unspent (the free room).
pub const GRID_OPEN_RESERVE: f64 = 0.04;

/// Levels at or above this form the SEALED HEAD: everything up to the young
/// end of level 1 (tiles of 4 units, 3.0 days). The best single save point in
/// the 2026-11-08 replays.
pub const GRID_HEAD_LEVEL: u32 = 1;

/// The deepest level the grid builds: `4^(GRID_MAX_LEVEL + 1)` units of
/// `GRID_UNIT_NS` still fit an `i128` of nanoseconds. A journal reaching
/// across the grid's origin (signed TAI before its epoch) never shares a
/// block with a later unit, so the level search stops here and an apex tile
/// holds what lies before the top level.
const GRID_MAX_LEVEL: u32 = 38;

/// The start of the level-`level` block holding unit `n`, in units:
/// `floor(n / 4^level) * 4^level`.
fn block(n: i128, level: u32) -> i128 {
    let width = 4i128.pow(level);
    n.div_euclid(width) * width
}

/// Range-time order: start ascending, end descending, then intrinsic id.
fn range_time_sort(entries: &mut [usize], spans: &[(i128, i128, Id)]) {
    entries.sort_by(|&a, &b| {
        spans[a]
            .0
            .cmp(&spans[b].0)
            .then(spans[b].1.cmp(&spans[a].1))
            .then(spans[a].2.cmp(&spans[b].2))
    });
}

/// One tile `[ta, tb)`: today's walk over the eligible memories that START in
/// it, against UNIFORM targets clipped to lived time (they begin at
/// `max(ta, earliest)`, so no target character falls before the journal
/// began). Returns the characters spent, at most `tile_space`.
#[allow(clippy::too_many_arguments)]
fn walk_tile(
    spans: &[(i128, i128, Id)],
    costs: &[usize],
    eligible: &[bool],
    selected: &mut [bool],
    cover: &mut Vec<usize>,
    earliest: i128,
    ta: i128,
    tb: i128,
    tile_space: usize,
) -> usize {
    let lived = ta.max(earliest);
    if tile_space == 0 || lived >= tb {
        return 0;
    }
    let candidate: Vec<bool> = spans
        .iter()
        .zip(eligible)
        .map(|(span, &ok)| ok && span.0 >= ta && span.0 < tb)
        .collect();
    let life = (tb - lived) as f64;
    recollection_walk(
        spans,
        costs,
        &candidate,
        selected,
        lived,
        &|cursor| life * cursor.min(tile_space) as f64 / tile_space as f64,
        tile_space,
        cover,
    )
}

/// The clock-closed geometric grid.
///
/// Let `n` be the unit holding the newest memory end. Level `l` spans units
/// `[block(n, l + 1), block(n, l))`: its tile count is the base-4 digit `l` of
/// `n`, at most three tiles of `4^l` units. The levels run up to the first
/// whose block reaches the earliest memory, and the open unit is
/// `[n * unit, newest end]`.
///
/// - Each closed level gets an equal share of `(1 - GRID_OPEN_RESERVE)` of the
///   budget: equal per 4x age band, as the log-age curve gives it. A level's
///   allowance (its share plus what coarser levels left unspent) is split
///   evenly over its tiles, oldest first, unspent passing on.
/// - Inside a tile today's walk ([`recollection_walk`]) chooses among the
///   eligible memories that START in the tile, against UNIFORM targets over
///   the tile clipped to lived time: they begin at `max(tile start, earliest
///   memory)`, so no target character is spent on time before the journal
///   began. Granularity therefore coarsens with depth by itself.
/// - The open unit is append-only: exactly the eligible memories that start in
///   it, in range-time order, with no filler. It has the reserve plus the free
///   room, and when that is not enough it drops its OLDEST memories first.
/// - Unused characters are left free.
/// - Every memory is a candidate at once: a late summary lands in its tile at
///   the next render.
///
/// Emitted order: levels coarsest first, then the open unit. `head` counts the
/// entries of levels `>= GRID_HEAD_LEVEL`, which lead the cover; they change
/// only when their own or a coarser base-4 digit of `n` changes, or when a
/// memory starting in one of their tiles arrives. Pure in the spans, costs,
/// eligibility and budget: no previous cover, observation time or clock.
pub fn select_grid_recollection_cut(
    spans: &[(i128, i128, Id)],
    costs: &[usize],
    eligible: &[bool],
    budget: usize,
) -> RecollectionCut {
    select_grid_recollection_cut_with(spans, costs, eligible, budget, LevelOneRule::Divide).0
}

/// How level [`GRID_HEAD_LEVEL`] (tiles of 3.0 days, whose young end is the
/// sealed head's end) sizes its tiles. EXPERIMENTAL (lane
/// cover-underfill-20261115): only [`LevelOneRule::Divide`] is the grid.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LevelOneRule {
    /// The grid as settled (b110455f): the level's allowance re-divided over
    /// the tiles the level holds now, oldest first, unspent passing on.
    #[default]
    Divide,
    /// EXPERIMENTAL UNDERFILL: tile `t` of this level has a FIXED space of
    /// `share / 3` plus a carry -- what coarser levels left unspent for tile
    /// 0, what tile `t - 1` left of its own space after that -- so a tile's
    /// space never depends on how many tiles the level holds yet. Whatever
    /// the level leaves unspent (including the `share / 3` of each tile not
    /// yet present) is left unused by the cover: it passes neither to level
    /// 0 nor to the open unit. Other levels keep [`Self::Divide`].
    Underfill,
}

/// What [`select_grid_recollection_cut_with`] reports beside the cut.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GridDetail {
    /// Allowance a rule deliberately left unused by the cover. Always zero
    /// for [`LevelOneRule::Divide`].
    pub withheld: usize,
}

/// [`select_grid_recollection_cut`] under an explicit level-1 rule. With
/// [`LevelOneRule::Divide`] it is exactly the grid.
pub fn select_grid_recollection_cut_with(
    spans: &[(i128, i128, Id)],
    costs: &[usize],
    eligible: &[bool],
    budget: usize,
    rule: LevelOneRule,
) -> (RecollectionCut, GridDetail) {
    let mut detail = GridDetail::default();
    assert_eq!(spans.len(), costs.len());
    assert_eq!(spans.len(), eligible.len());
    let mut out = RecollectionCut {
        cover: Vec::new(),
        used: 0,
        head: 0,
    };
    let (Some(earliest), Some(latest)) = (
        spans.iter().map(|span| span.0).min(),
        spans.iter().map(|span| span.1).max(),
    ) else {
        return (out, detail);
    };
    if budget == 0 {
        return (out, detail);
    }
    let unit = GRID_UNIT_NS;
    let n = latest.div_euclid(unit);
    let first = earliest.div_euclid(unit);
    let mut top = 0u32;
    while top < GRID_MAX_LEVEL && block(n, top + 1) > first {
        top += 1;
    }
    let apex = block(n, top + 1) > first;
    let regions = top as f64 + 1.0 + if apex { 1.0 } else { 0.0 };
    let share = ((1.0 - GRID_OPEN_RESERVE) * budget as f64 / regions).floor() as usize;
    let mut selected = vec![false; spans.len()];
    let mut carry = 0usize;
    if apex {
        let allowance = share.min(budget - out.used);
        let (ta, tb) = (first * unit, block(n, top + 1) * unit);
        let spent = walk_tile(
            spans,
            costs,
            eligible,
            &mut selected,
            &mut out.cover,
            earliest,
            ta,
            tb,
            allowance,
        );
        out.used += spent;
        carry = allowance - spent;
        out.head = out.cover.len();
    }
    for level in (0..=top).rev() {
        let width = 4i128.pow(level);
        let (lo, hi) = (block(n, level + 1), block(n, level));
        let tiles = ((hi - lo) / width) as usize;
        let allowance = share.saturating_add(carry).min(budget - out.used);
        let underfill = rule == LevelOneRule::Underfill && level == GRID_HEAD_LEVEL;
        let mut pool = carry;
        let mut spent = 0usize;
        for t in 0..tiles {
            let ta = (lo + t as i128 * width) * unit;
            let tb = ta + width * unit;
            let tile_space = if underfill {
                (share / 3).saturating_add(pool).min(allowance - spent)
            } else {
                (allowance - spent) / (tiles - t)
            };
            let got = walk_tile(
                spans,
                costs,
                eligible,
                &mut selected,
                &mut out.cover,
                earliest,
                ta,
                tb,
                tile_space,
            );
            spent += got;
            pool = tile_space - got;
        }
        out.used += spent;
        carry = allowance - spent;
        if underfill {
            detail.withheld = carry;
            carry = 0;
        }
        if level >= GRID_HEAD_LEVEL {
            out.head = out.cover.len();
        }
    }
    let room = (budget - out.used).saturating_sub(detail.withheld);
    let origin = n * unit;
    let mut members: Vec<usize> = (0..spans.len())
        .filter(|&i| eligible[i] && costs[i] > 0 && spans[i].0 >= origin)
        .collect();
    range_time_sort(&mut members, spans);
    let mut kept: usize = members.iter().map(|&i| costs[i]).sum();
    let mut cut = 0usize;
    while kept > room {
        kept -= costs[members[cut]];
        cut += 1;
    }
    out.cover.extend_from_slice(&members[cut..]);
    out.used += kept;
    (out, detail)
}

#[cfg(test)]
mod tests {

    #[test]
    fn a_journal_across_the_grid_origin_terminates_and_keeps_both_sides() {
        // Signed TAI allows times before the grid's origin; a unit before 0
        // and one after it never share a block, so only the level cap and
        // the apex tile end the level search.
        let u = GRID_UNIT_NS;
        let spans = vec![(-u, 0, id(1)), (0, u, id(2))];
        let costs = vec![100, 100];
        let cut = select_grid_recollection_cut(&spans, &costs, &[true, true], 10_000);
        let mut picked = cut.cover.clone();
        picked.sort();
        assert_eq!(picked, vec![0, 1]);
        assert!(cut.used <= 10_000);
    }
    use super::super::*;
    use super::*;

    const H: i128 = 3600 * 1_000_000_000;
    const DAY: i128 = 24 * H;
    /// Far from zero so the absolute grid is exercised, aligned to nothing.
    const BASE: i128 = 20_000 * DAY + 7 * H + 123_456_789;
    const BUDGET: usize = 20_000;

    fn id(k: u128) -> Id {
        Id::new(u128::to_be_bytes(0x6A000000000000000000000000000000 + k)).expect("nonzero")
    }

    /// Hourly memories over `[0, hours)` plus one arc per day for the first
    /// `arc_days` days, with positional ids (a memory keeps its id when later
    /// ones are appended).
    fn journal(hours: i128, arc_days: i128) -> Vec<(i128, i128, Id)> {
        let mut spans: Vec<_> = (0..hours)
            .map(|h| (BASE + h * H, BASE + (h + 1) * H, id(h as u128 + 1)))
            .collect();
        for d in 0..arc_days {
            spans.push((
                BASE + d * DAY,
                BASE + (d + 1) * DAY,
                id(1_000_000 + d as u128),
            ));
        }
        spans
    }

    /// Costs that vary with the memory, so the walk's fit test is exercised.
    fn costs(spans: &[(i128, i128, Id)]) -> Vec<usize> {
        spans
            .iter()
            .map(|span| 120 + ((span.0 - BASE).div_euclid(H) % 7) as usize * 40)
            .collect()
    }

    fn cut_at(spans: &[(i128, i128, Id)], budget: usize) -> RecollectionCut {
        select_grid_recollection_cut(spans, &costs(spans), &vec![true; spans.len()], budget)
    }

    fn newest_unit(spans: &[(i128, i128, Id)]) -> i128 {
        spans
            .iter()
            .map(|s| s.1)
            .max()
            .unwrap()
            .div_euclid(GRID_UNIT_NS)
    }

    /// The level a memory starting at `start` belongs to, `None` for the open
    /// unit.
    fn level_of(n: i128, start: i128) -> Option<u32> {
        let u = start.div_euclid(GRID_UNIT_NS);
        (0..60).find(|&l| block(n, l + 1) <= u && u < block(n, l))
    }

    /// Per level, the selected ids in emitted order.
    fn levels(spans: &[(i128, i128, Id)], cut: &RecollectionCut) -> BTreeMap<u32, Vec<Id>> {
        let n = newest_unit(spans);
        let mut levels: BTreeMap<u32, Vec<Id>> = BTreeMap::new();
        for &i in &cut.cover {
            if let Some(level) = level_of(n, spans[i].0) {
                levels.entry(level).or_default().push(spans[i].2);
            }
        }
        levels
    }

    use std::collections::BTreeMap;

    /// Levels come coarsest first, then the open unit; the head is exactly the
    /// entries up to the young end of level 1.
    #[test]
    fn levels_lead_coarsest_first_and_the_head_ends_at_level_one() {
        let spans = journal(60 * 24, 55);
        let cut = cut_at(&spans, BUDGET);
        let n = newest_unit(&spans);
        assert!(cut.used <= BUDGET);
        let order: Vec<Option<u32>> = cut.cover.iter().map(|&i| level_of(n, spans[i].0)).collect();
        // Non-increasing level, the open unit (None) last.
        let rank = |l: &Option<u32>| l.map_or(-1, |l| l as i64);
        assert!(
            order.windows(2).all(|w| rank(&w[0]) >= rank(&w[1])),
            "{order:?}"
        );
        let edge = block(n, GRID_HEAD_LEVEL) * GRID_UNIT_NS;
        let head = cut.cover.iter().take_while(|&&i| spans[i].0 < edge).count();
        assert_eq!(cut.head, head);
        assert!(cut.head > 0 && cut.head < cut.cover.len());
        assert!(cut.cover[head..].iter().all(|&i| spans[i].0 >= edge));
    }

    /// Ordinary appends: a level changes only when its own base-4 digit of `n`
    /// or a coarser one changes.
    #[test]
    fn a_level_changes_only_with_its_own_or_a_coarser_digit() {
        let mut previous: Option<(i128, BTreeMap<u32, Vec<Id>>)> = None;
        let (mut held, mut moved) = (0usize, 0usize);
        for hours in 40 * 24..70 * 24 {
            let spans = journal(hours, 35);
            let now = (newest_unit(&spans), levels(&spans, &cut_at(&spans, BUDGET)));
            if let Some((was_n, was)) = previous.take() {
                for level in 0..12u32 {
                    let w = 4i128.pow(level);
                    if was_n.div_euclid(w) == now.0.div_euclid(w) {
                        held += 1;
                        assert_eq!(
                            was.get(&level),
                            now.1.get(&level),
                            "level {level} changed under unchanged digits at hour {hours}"
                        );
                    } else if was.get(&level) != now.1.get(&level) {
                        moved += 1;
                    }
                }
            }
            previous = Some(now);
        }
        assert!(held > 1_000 && moved > 0, "held {held}, moved {moved}");
    }

    /// Targets cover lived time only. A tile that begins long before the first
    /// memory spreads its picks over the lived part; unclipped uniform targets
    /// would spend most characters before the journal began and snap them all
    /// to its first memories.
    #[test]
    fn tile_targets_never_land_before_the_earliest_memory() {
        // Level 3 holds two 64-unit tiles, and the journal starts three
        // quarters into the older one.
        let tile = 64 * GRID_UNIT_NS;
        let ta = (BASE.div_euclid(4 * tile) + 1) * 4 * tile;
        let start = ta + 3 * tile / 4;
        let hours = ((ta + 2 * tile + 5 * H) - start) / H;
        let spans: Vec<_> = (0..hours)
            .map(|h| (start + h * H, start + (h + 1) * H, id(h as u128 + 1)))
            .collect();
        let costs = vec![150; spans.len()];
        let eligible = vec![true; spans.len()];
        let cut = select_grid_recollection_cut(&spans, &costs, &eligible, 40_000);
        let tb = ta + tile;
        let in_tile = |i: &usize| spans[*i].0 < tb;
        let picks: Vec<usize> = cut.cover.iter().copied().filter(in_tile).collect();
        assert!(picks.len() >= 8, "{} picks", picks.len());
        let lived = tb - start;
        let early = |picks: &[usize]| {
            picks
                .iter()
                .filter(|&&i| spans[i].0 - start < lived / 2)
                .count() as f64
                / picks.len() as f64
        };
        assert!(
            early(&picks) <= 0.6,
            "grid early fraction {}",
            early(&picks)
        );
        // The same walk with targets from the tile start instead.
        let space: usize = picks.iter().map(|&i| costs[i]).sum();
        let candidate: Vec<bool> = (0..spans.len()).map(|i| in_tile(&i)).collect();
        let mut unclipped = Vec::new();
        let life = tile as f64;
        recollection_walk(
            &spans,
            &costs,
            &candidate,
            &mut vec![false; spans.len()],
            ta,
            &|cursor| life * cursor.min(space) as f64 / space as f64,
            space,
            &mut unclipped,
        );
        assert!(
            early(&unclipped) > 0.6,
            "unclipped early fraction {}",
            early(&unclipped)
        );
    }

    /// The open unit holds exactly the memories that start in it, in
    /// range-time order, and while `n` holds the cover only grows at its end.
    #[test]
    fn the_open_unit_is_append_only_without_filler() {
        let mut previous: Option<(i128, Vec<Id>)> = None;
        let mut grew = 0;
        for hours in 50 * 24..52 * 24 {
            let spans = journal(hours, 45);
            let cut = cut_at(&spans, 150_000);
            let n = newest_unit(&spans);
            let origin = n * GRID_UNIT_NS;
            let mut members: Vec<usize> =
                (0..spans.len()).filter(|&i| spans[i].0 >= origin).collect();
            range_time_sort(&mut members, &spans);
            let closed = cut.cover.len() - members.len();
            assert_eq!(
                &cut.cover[closed..],
                &members[..],
                "open unit at hour {hours}"
            );
            // No filler: the rest are closed-level picks, though older
            // memories went unpicked and room was left free.
            assert!(cut.cover[..closed].iter().all(|&i| spans[i].0 < origin));
            assert!(cut.cover.len() < spans.len());
            let ids: Vec<Id> = cut.cover.iter().map(|&i| spans[i].2).collect();
            if let Some((was_n, was)) = previous.take() {
                if was_n == n {
                    assert!(ids.starts_with(&was), "cover rewritten at hour {hours}");
                    grew += 1;
                }
            }
            previous = Some((n, ids));
        }
        assert!(grew > 10, "{grew}");
    }

    /// When the reserve and the free room are not enough, the open unit drops
    /// its OLDEST memories, never the newest, and never exceeds the room.
    #[test]
    fn the_open_unit_cuts_its_oldest_memories_first() {
        let unit_start = (BASE.div_euclid(GRID_UNIT_NS) + 1) * GRID_UNIT_NS;
        let spans: Vec<_> = (0..20)
            .map(|k| {
                let start = unit_start + k * H / 2;
                (start, start + H / 2, id(k as u128 + 1))
            })
            .collect();
        let costs = vec![100; spans.len()];
        let eligible = vec![true; spans.len()];
        let cut = select_grid_recollection_cut(&spans, &costs, &eligible, 1_050);
        // Everything is in the open unit; ten fit, and they are the newest.
        assert_eq!(cut.head, 0);
        assert_eq!(cut.used, 1_000);
        assert_eq!(cut.cover, (10..20).collect::<Vec<_>>());
        // An ineligible member is never shown and frees its room.
        let mut eligible = eligible;
        eligible[15] = false;
        let cut = select_grid_recollection_cut(&spans, &costs, &eligible, 1_050);
        assert_eq!(cut.cover, [9, 10, 11, 12, 13, 14, 16, 17, 18, 19]);
    }

    /// `created_at` is not an input: permuting every memory's observation time
    /// leaves the grid cover byte-identical, sealed marks included.
    #[test]
    fn permuting_created_at_leaves_the_grid_cover_byte_identical() {
        use crate::memory::{chunk_fragment, ChunkDraft, ChunkDraftContent};
        use std::collections::BTreeSet;
        use triblespace::core::blob::MemoryBlobStore;
        use triblespace::core::repo::SnapshotSource;
        let point = |key: i128| {
            let epoch = key_to_epoch(key);
            (epoch, epoch).try_to_inline().unwrap()
        };
        let spans = journal(30 * 24, 25);
        let render = |shift: usize| {
            let mut facts = TribleSet::new();
            let mut blobs = MemoryBlobStore::new();
            for (k, &(start, end, _)) in spans.iter().enumerate() {
                // Observation times are a rotation of the range ends.
                let observed = spans[(k + shift) % spans.len()].1;
                let (fragment, _) = chunk_fragment(ChunkDraft {
                    content: ChunkDraftContent::Text(format!("memory {k}, {}.", "x".repeat(k % 9))),
                    start_at: point(start),
                    end_at: point(end),
                    tags: BTreeSet::new(),
                    references: BTreeSet::new(),
                    about_exec_result: None,
                    about_archive_message: None,
                    observed_at: BTreeSet::from([point(observed)]),
                    aliases: BTreeSet::new(),
                })
                .unwrap();
                let (chunk_facts, chunk_blobs) = fragment.into_facts_and_blobs();
                facts += chunk_facts;
                blobs.union(chunk_blobs);
            }
            let reader = blobs.snapshot().unwrap();
            let mut options = CoverOpts::plain(6_000);
            options.selector = Selector::Grid;
            options.sealed_head = true;
            let report = render_cover_report(&facts, &reader, &options, &mut |_| Ok(None)).unwrap();
            let raw = collect_chunk_spans(&facts);
            let (_, classes) = recollection_classes(&raw);
            (
                report,
                recollection_class_observations(&facts, &raw, &classes).0,
            )
        };
        let (base, base_observed) = render(0);
        assert!(base.entries.iter().any(|entry| entry.sealed));
        for shift in [1, 17, 400] {
            let (moved, observed) = render(shift);
            assert_ne!(observed, base_observed, "the permutation moved created_at");
            assert_eq!(moved.text, base.text);
            assert_eq!(moved.entries, base.entries);
        }
    }

    /// Hours, three-hour arcs, days and weeks over 75 days, uneven costs.
    fn proto_journal() -> (Vec<(i128, i128, Id)>, Vec<usize>) {
        const H: i128 = 3600 * 1_000_000_000;
        let base: i128 = 20_000 * 24 * H + 7 * H + 123_456_789;
        let id =
            |k: u128| Id::new(u128::to_be_bytes(0x6B000000000000000000000000000000 + k)).unwrap();
        let mut spans = Vec::new();
        let mut k = 0u128;
        let mut push = |spans: &mut Vec<(i128, i128, Id)>, s: i128, e: i128| {
            k += 1;
            spans.push((s, e, id(k)));
        };
        for h in 0..75 * 24 {
            push(&mut spans, base + h * H, base + (h + 1) * H);
        }
        for t in 0..60 * 8 {
            push(&mut spans, base + 3 * t * H, base + 3 * (t + 1) * H);
        }
        for d in 0..50 {
            push(&mut spans, base + 24 * d * H, base + 24 * (d + 1) * H);
        }
        for w in 0..9 {
            push(&mut spans, base + 168 * w * H, base + 168 * (w + 1) * H);
        }
        let costs = (0..spans.len()).map(|i| 80 + (i * 37) % 300).collect();
        (spans, costs)
    }

    /// The prototype's "tiled 2^16 grid, equal shares, open from free room,
    /// oldest cut" (faculties cover-sealed-20261108 42d862a2,
    /// `select_tiled_recollection_cut`) selected exactly this cover, in this
    /// order, on this journal at 7,000 characters. The same comparison against
    /// a verbatim copy of the prototype agreed at budgets 0, 1, 500, 7,000,
    /// 30,000, 120,000 and 1,000,000 and on ten more journals (lane
    /// cover-select-20261108, handoff/proto_check-scratch.rs).
    #[test]
    fn the_prototype_selected_the_same_cover() {
        const PROTOTYPE: [usize; 50] = [
            0, 1, 2, 3, 4, 5, 6, 57, 130, 2331, 1930, 446, 2303, 2011, 673, 2311, 2084, 2100, 973,
            1046, 1192, 2206, 1265, 2230, 2246, 1371, 1411, 1484, 1500, 1541, 1573, 1606, 1646,
            1679, 1711, 1756, 1760, 1761, 1762, 1765, 1768, 1769, 1775, 1778, 1780, 1784, 1785,
            1786, 1787, 1799,
        ];
        let (spans, costs) = proto_journal();
        let cut = select_grid_recollection_cut(&spans, &costs, &vec![true; spans.len()], 7_000);
        assert_eq!(cut.cover, PROTOTYPE);
        assert_eq!(cut.used, 6_804);
    }

    /// faculties b110455f's `select_grid_recollection_cut`, verbatim but for
    /// its name: the rule-parameterised function must reproduce it exactly
    /// under [`LevelOneRule::Divide`].
    #[allow(clippy::all)]
    fn b110455f_select(
        spans: &[(i128, i128, Id)],
        costs: &[usize],
        eligible: &[bool],
        budget: usize,
    ) -> RecollectionCut {
        assert_eq!(spans.len(), costs.len());
        assert_eq!(spans.len(), eligible.len());
        let mut out = RecollectionCut {
            cover: Vec::new(),
            used: 0,
            head: 0,
        };
        let (Some(earliest), Some(latest)) = (
            spans.iter().map(|span| span.0).min(),
            spans.iter().map(|span| span.1).max(),
        ) else {
            return out;
        };
        if budget == 0 {
            return out;
        }
        let unit = GRID_UNIT_NS;
        let n = latest.div_euclid(unit);
        let first = earliest.div_euclid(unit);
        let mut top = 0u32;
        while top < GRID_MAX_LEVEL && block(n, top + 1) > first {
            top += 1;
        }
        let apex = block(n, top + 1) > first;
        let regions = top as f64 + 1.0 + if apex { 1.0 } else { 0.0 };
        let share = ((1.0 - GRID_OPEN_RESERVE) * budget as f64 / regions).floor() as usize;
        let mut selected = vec![false; spans.len()];
        let mut carry = 0usize;
        if apex {
            let allowance = share.min(budget - out.used);
            let (ta, tb) = (first * unit, block(n, top + 1) * unit);
            let spent = walk_tile(
                spans,
                costs,
                eligible,
                &mut selected,
                &mut out.cover,
                earliest,
                ta,
                tb,
                allowance,
            );
            out.used += spent;
            carry = allowance - spent;
            out.head = out.cover.len();
        }
        for level in (0..=top).rev() {
            let width = 4i128.pow(level);
            let (lo, hi) = (block(n, level + 1), block(n, level));
            let tiles = ((hi - lo) / width) as usize;
            let allowance = share.saturating_add(carry).min(budget - out.used);
            let mut spent = 0usize;
            for t in 0..tiles {
                let ta = (lo + t as i128 * width) * unit;
                let tb = ta + width * unit;
                let tile_space = (allowance - spent) / (tiles - t);
                spent += walk_tile(
                    spans,
                    costs,
                    eligible,
                    &mut selected,
                    &mut out.cover,
                    earliest,
                    ta,
                    tb,
                    tile_space,
                );
            }
            out.used += spent;
            carry = allowance - spent;
            if level >= GRID_HEAD_LEVEL {
                out.head = out.cover.len();
            }
        }
        let room = budget - out.used;
        let origin = n * unit;
        let mut members: Vec<usize> = (0..spans.len())
            .filter(|&i| eligible[i] && costs[i] > 0 && spans[i].0 >= origin)
            .collect();
        range_time_sort(&mut members, spans);
        let mut kept: usize = members.iter().map(|&i| costs[i]).sum();
        let mut cut = 0usize;
        while kept > room {
            kept -= costs[members[cut]];
            cut += 1;
        }
        out.cover.extend_from_slice(&members[cut..]);
        out.used += kept;
        out
    }

    /// EXPERIMENTAL lane cover-underfill-20261115: CURRENT is bit-for-bit
    /// b110455f on every journal shape and budget tried.
    #[test]
    fn divide_rule_is_b110455f_exactly() {
        let (proto, proto_costs) = proto_journal();
        let mut cases: Vec<(Vec<(i128, i128, Id)>, Vec<usize>)> =
            vec![(proto.clone(), proto_costs)];
        for hours in [3, 30, 24 * 5, 24 * 13, 24 * 40 + 7, 24 * 61 + 19] {
            let spans = journal(hours, hours / 24);
            let c = costs(&spans);
            cases.push((spans, c));
        }
        let mut compared = 0;
        for (spans, c) in &cases {
            for budget in [0, 1, 500, 7_000, 30_000, 120_000, 1_000_000] {
                for mask in 0..3usize {
                    let eligible: Vec<bool> = (0..spans.len())
                        .map(|i| mask == 0 || i % (mask + 2) != 0)
                        .collect();
                    let want = b110455f_select(spans, c, &eligible, budget);
                    let (got, detail) = select_grid_recollection_cut_with(
                        spans,
                        c,
                        &eligible,
                        budget,
                        LevelOneRule::Divide,
                    );
                    assert_eq!(got, want, "budget {budget} mask {mask}");
                    assert_eq!(detail.withheld, 0);
                    assert_eq!(
                        select_grid_recollection_cut(spans, c, &eligible, budget),
                        want
                    );
                    compared += 1;
                }
            }
        }
        assert_eq!(compared, 7 * 7 * 3);
    }

    // ---- EXPERIMENTAL lane cover-underfill-20261115: synthetic traces ----

    const RULES: [(&str, LevelOneRule); 2] = [
        ("CURRENT", LevelOneRule::Divide),
        ("UNDERFILL", LevelOneRule::Underfill),
    ];
    const TRACE_BUDGET: usize = 60_000;

    fn cut_with(spans: &[(i128, i128, Id)], rule: LevelOneRule) -> RecollectionCut {
        select_grid_recollection_cut_with(
            spans,
            &costs(spans),
            &vec![true; spans.len()],
            TRACE_BUDGET,
            rule,
        )
        .0
    }

    /// Rendered bytes of the cover's entries `[..upto]`, each exactly as the
    /// render writes one entry, with a synthetic body per memory.
    fn bytes(spans: &[(i128, i128, Id)], cover: &[usize]) -> Vec<u8> {
        cover
            .iter()
            .flat_map(|&i| {
                let (s, e, id) = spans[i];
                crate::memory_cover::replay_grid::entry_text(s, e, Some(&format!("memory {id:x}")))
                    .into_bytes()
            })
            .collect()
    }

    /// `(old head bytes, new head bytes, first differing byte or None,
    /// new head starts with the old head)`.
    fn head_change(
        before: &[(i128, i128, Id)],
        after: &[(i128, i128, Id)],
        rule: LevelOneRule,
    ) -> (usize, usize, Option<usize>, bool) {
        let (a, b) = (cut_with(before, rule), cut_with(after, rule));
        let (ha, hb) = (
            bytes(before, &a.cover[..a.head]),
            bytes(after, &b.cover[..b.head]),
        );
        let first = ha
            .iter()
            .zip(&hb)
            .position(|(x, y)| x != y)
            .or(if ha.len() == hb.len() {
                None
            } else {
                Some(ha.len().min(hb.len()))
            });
        (ha.len(), hb.len(), first, hb.starts_with(&ha))
    }

    /// With UNDERFILL a new 3-day tile EXTENDS the head; the saved prefix
    /// survives. With CURRENT the same step re-divides the level and the
    /// older 3-day tiles are re-chosen.
    #[test]
    fn underfill_a_new_three_day_tile_extends_the_head() {
        let mut current_broke = 0;
        let mut digit_one_steps = 0;
        for hours in 40 * 24..90 * 24 {
            let (before, after) = (journal(hours, 35), journal(hours + 1, 35));
            let (was, now) = (newest_unit(&before), newest_unit(&after));
            if was.div_euclid(16) != now.div_euclid(16) || was.div_euclid(4) == now.div_euclid(4) {
                continue;
            }
            digit_one_steps += 1;
            let (_, _, _, under_ok) = head_change(&before, &after, LevelOneRule::Underfill);
            assert!(under_ok, "UNDERFILL head not extended at hour {hours}");
            let (old, new, first, current_ok) = head_change(&before, &after, LevelOneRule::Divide);
            if !current_ok {
                current_broke += 1;
            }
            eprintln!(
                "TRACE new-3day-tile hour {hours}: CURRENT head {old}->{new} bytes, first change at {first:?}, extends {current_ok}; UNDERFILL extends {under_ok}"
            );
        }
        assert!(digit_one_steps >= 5, "{digit_one_steps}");
        assert!(
            current_broke > 0,
            "CURRENT never broke on {digit_one_steps} steps"
        );
    }

    /// An append in the open unit changes no head byte under either rule.
    #[test]
    fn an_append_in_the_open_unit_changes_no_head_byte() {
        let base = journal(55 * 24 + 3, 45);
        let n = newest_unit(&base);
        let origin = n * GRID_UNIT_NS;
        let mut after = base.clone();
        let end = base.iter().map(|s| s.1).max().unwrap();
        after.push(((end - H / 2).max(origin), end, id(7_000_001)));
        for (name, rule) in RULES {
            let (old, new, first, extends) = head_change(&base, &after, rule);
            let (a, b) = (cut_with(&base, rule), cut_with(&after, rule));
            eprintln!(
                "TRACE open-append {name}: head {old}->{new} bytes, first change {first:?}; cover extends {}",
                b.cover.starts_with(&a.cover)
            );
            assert_eq!(first, None);
            assert!(extends);
            assert!(b.cover.starts_with(&a.cover));
        }
    }

    /// A retrospective summary of an older span lands in the tile its start
    /// falls in. Bytes before that tile stay; under both rules the head
    /// changes from that tile on.
    #[test]
    fn a_retrospective_summary_changes_the_head_from_its_own_tile() {
        let base = journal(62 * 24 + 5, 45);
        let n = newest_unit(&base);
        for (label, days_back) in [("level-1 tile", 4i128), ("level-2+ tile", 20)] {
            let end = base.iter().map(|s| s.1).max().unwrap();
            let start = (end - days_back * DAY).div_euclid(H) * H;
            let level = level_of(n, start).expect("closed level");
            let mut after = base.clone();
            after.push((start, start + 2 * DAY, id(8_000_000 + days_back as u128)));
            assert_eq!(newest_unit(&after), n);
            for (name, rule) in RULES {
                let (a, b) = (cut_with(&base, rule), cut_with(&after, rule));
                // Head entries from levels coarser than the summary's.
                let coarser = |c: &RecollectionCut, spans: &[(i128, i128, Id)]| {
                    c.cover[..c.head]
                        .iter()
                        .take_while(|&&i| level_of(n, spans[i].0).is_some_and(|l| l > level))
                        .map(|&i| spans[i].2)
                        .collect::<Vec<_>>()
                };
                assert_eq!(coarser(&a, &base), coarser(&b, &after), "{name} {label}");
                let (old, new, first, extends) = head_change(&base, &after, rule);
                let picked = b
                    .cover
                    .iter()
                    .any(|&i| after[i].2 == id(8_000_000 + days_back as u128));
                eprintln!(
                    "TRACE retrospective {label} (level {level}) {name}: head {old}->{new} bytes, first change {first:?}, extends {extends}, summary selected {picked}; coarser levels unchanged"
                );
            }
        }
    }

    /// A late memory with an older authored time, in both arrival orders:
    /// the two histories converge to the same render, and each arrival
    /// changes the head only from the tile it starts in.
    #[test]
    fn a_late_older_memory_converges_in_both_arrival_orders() {
        let base = journal(58 * 24 + 11, 45);
        let n = newest_unit(&base);
        let end = base.iter().map(|s| s.1).max().unwrap();
        let recent = ((end - H / 3).max(n * GRID_UNIT_NS), end, id(9_000_001));
        let start = (end - 5 * DAY).div_euclid(H) * H;
        let late = (start, start + 3 * H, id(9_000_002));
        let level = level_of(n, late.0);
        for (name, rule) in RULES {
            let with = |extra: &[(i128, i128, Id)]| {
                let mut s = base.clone();
                s.extend_from_slice(extra);
                s
            };
            let (ab, ba) = (with(&[recent, late]), with(&[late, recent]));
            let (x, y) = (cut_with(&ab, rule), cut_with(&ba, rule));
            let ids = |c: &RecollectionCut, s: &[(i128, i128, Id)]| {
                c.cover.iter().map(|&i| s[i].2).collect::<Vec<_>>()
            };
            assert_eq!(ids(&x, &ab), ids(&y, &ba), "{name}");
            assert_eq!(bytes(&ab, &x.cover), bytes(&ba, &y.cover), "{name}");
            assert_eq!((x.used, x.head), (y.used, y.head));
            for (order, first, second) in [
                ("recent-then-late", recent, late),
                ("late-then-recent", late, recent),
            ] {
                let one = with(&[first]);
                let two = with(&[first, second]);
                let a = head_change(&base, &one, rule);
                let b = head_change(&one, &two, rule);
                eprintln!(
                    "TRACE late-arrival {name} {order} (late memory at level {level:?}): first arrival head {}->{} first change {:?}; second arrival head {}->{} first change {:?}",
                    a.0, a.1, a.2, b.0, b.1, b.2
                );
                // The recent memory alone never touches the head.
                if first == recent {
                    assert_eq!(a.2, None, "{name}");
                } else {
                    assert_eq!(b.2, None, "{name}");
                }
            }
        }
    }
}
