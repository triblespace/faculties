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
    while block(n, top + 1) > first {
        top += 1;
    }
    let share = ((1.0 - GRID_OPEN_RESERVE) * budget as f64 / (top as f64 + 1.0)).floor() as usize;
    let mut selected = vec![false; spans.len()];
    let mut carry = 0usize;
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
            let lived = ta.max(earliest);
            if tile_space == 0 || lived >= tb {
                continue;
            }
            let candidate: Vec<bool> = spans
                .iter()
                .zip(eligible)
                .map(|(span, &ok)| ok && span.0 >= ta && span.0 < tb)
                .collect();
            let life = (tb - lived) as f64;
            spent += recollection_walk(
                spans,
                costs,
                &candidate,
                &mut selected,
                lived,
                &|cursor| life * cursor.min(tile_space) as f64 / tile_space as f64,
                tile_space,
                &mut out.cover,
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

#[cfg(test)]
mod tests {
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
}
