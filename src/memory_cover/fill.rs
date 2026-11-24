//! Filling a cover.
//!
//! A cover wants about four pieces of every age: four of about 18 hours, four
//! of 3 days, four of 12 days, and so on back to the first memory. Each age is
//! a bucket: "we aim for 4, but we compact when its twice our goal", "basically
//! a low and high watermark for each bucket". When a bucket would hold its
//! eighth piece, its oldest four fold into one piece of the next bucket and
//! four are left. Folding only keeps room: it widens the
//! time a piece stands for, and the memories that fit a piece are still chosen
//! by their own grain.
//!
//! Pieces complete one at a time by lived time, so how many a bucket holds
//! follows from the moment alone, with nothing remembered between calls. A
//! memory's length and the time it was written never move a piece.

use super::{Id, MOMENT_NS};

/// The low mark: how many pieces a bucket is left with, and how many pieces of
/// one bucket make a piece of the next.
const LOW: i128 = 4;

/// The high mark: a bucket folds when it would hold this many, so it holds
/// `LOW` to `HIGH - 1`.
const HIGH: i128 = 2 * LOW;

/// The width of the bottom bucket's pieces: 2^16 s, about 18.2 hours, on an
/// absolute clock (multiples since the TAI epoch).
pub const PIECE_NS: i128 = (1i128 << 16) * 1_000_000_000;

/// The share of the budget the buckets leave to the open piece, the one being
/// lived now. It also takes whatever room they don't use.
pub const OPEN_RESERVE: f64 = 0.04;

/// The lossy recollection selected for one reader, in time order: the oldest
/// bucket first, then the open piece.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecollectionCut {
    pub cover: Vec<usize>,
    pub used: usize,
    /// How many leading entries stand in finished pieces. They stay as they
    /// are until another piece finishes or a memory that starts in one of them
    /// arrives, so a consumer can keep them as one prefix.
    pub finished: usize,
}

/// Where `bucket` ends, counted in bottom pieces, when piece `now` is open.
///
/// The bottom bucket ends where the open piece begins. A higher one ends one
/// of its pieces earlier than it could: that piece's worth still lies in the
/// bucket below, as its four smaller parts. So the bottom lists 4 to 7 pieces,
/// and every bucket above lists 3 to 6 and has one more still finer.
fn bucket_end(now: i128, bucket: u32) -> i128 {
    let width = LOW.pow(bucket);
    match bucket {
        0 => now,
        _ => (now.div_euclid(width) - 1) * width,
    }
}

/// The most pieces `bucket` lists: all but the fold's eighth, and above the
/// bottom all but the one still finer too.
fn most_listed(bucket: u32) -> usize {
    (if bucket == 0 { HIGH - 1 } else { HIGH - 2 }) as usize
}

/// Fill one piece `[start, end)` with the eligible memories that START in it,
/// `space` characters spread evenly over its lived time, and return what it
/// spent.
///
/// Each candidate's charged size turns the spot reached so far into one ideal
/// stretch of time, and the memory whose own start and end match it best is
/// taken. The time before the journal's first memory is not lived, so nothing
/// is spent on it. The first best memory that doesn't fit ends the piece.
#[allow(clippy::too_many_arguments)]
fn fill_piece(
    spans: &[(i128, i128, Id)],
    costs: &[usize],
    eligible: &[bool],
    earliest: i128,
    (start, end): (i128, i128),
    space: usize,
    cover: &mut Vec<usize>,
) -> usize {
    let lived = start.max(earliest);
    if space == 0 || lived >= end {
        return 0;
    }
    let time_at = |spot: usize| (end - lived) as f64 * spot.min(space) as f64 / space as f64;
    let mut candidates: Vec<usize> = (0..spans.len())
        .filter(|&i| eligible[i] && costs[i] > 0 && (start..end).contains(&spans[i].0))
        .collect();
    let mut used = 0usize;
    while used < space {
        let error = |&i: &usize| {
            let from = (spans[i].0 - lived) as f64;
            let width = spans[i].1.saturating_sub(spans[i].0).max(MOMENT_NS) as f64;
            let start_error = from - time_at(used);
            let end_error = from + width - time_at(used.saturating_add(costs[i]));
            start_error.mul_add(start_error, end_error * end_error)
        };
        let Some((at, &pick)) = candidates.iter().enumerate().min_by(|(_, a), (_, b)| {
            error(a)
                .total_cmp(&error(b))
                .then(spans[**a].2.cmp(&spans[**b].2))
        }) else {
            break;
        };
        if used.saturating_add(costs[pick]) > space {
            break;
        }
        used += costs[pick];
        cover.push(pick);
        candidates.swap_remove(at);
    }
    used
}

/// Fill one reader's cover.
///
/// Every bucket has the same share of the budget: each fourfold step of age
/// weighs the same. Every piece of a bucket has the same size, the share over
/// the most pieces the bucket lists, whatever it lists now. What a piece
/// doesn't use goes to the next NEWER piece of its own bucket and no further,
/// so a piece that finishes changes none before it. The budget is thus the
/// high mark: a cover uses about three quarters of it on average, and the rest
/// is room for what is being lived.
///
/// The open piece is append-only: the eligible memories that start in it, in
/// time order. When its room is too small its OLDEST memories go first.
///
/// A memory written late about the past is a candidate for its piece at the
/// next render. Each exact-span structural class may appear once. Pure in the
/// spans, costs, eligibility and budget: no earlier cover, observation time or
/// clock.
pub fn select_recollection_cut(
    spans: &[(i128, i128, Id)],
    costs: &[usize],
    eligible: &[bool],
    budget: usize,
) -> RecollectionCut {
    assert_eq!(spans.len(), costs.len());
    assert_eq!(spans.len(), eligible.len());
    let mut cut = RecollectionCut {
        cover: Vec::new(),
        used: 0,
        finished: 0,
    };
    let (Some(earliest), Some(latest)) = (
        spans.iter().map(|span| span.0).min(),
        spans.iter().map(|span| span.1).max(),
    ) else {
        return cut;
    };
    let now = latest.div_euclid(PIECE_NS);
    let first = earliest.div_euclid(PIECE_NS);
    let mut buckets = 1u32;
    while bucket_end(now, buckets) > first {
        buckets += 1;
    }
    // When the journal reaches a new bucket the share, and so every piece's
    // size, changes once.
    let share = ((1.0 - OPEN_RESERVE) * budget as f64 / buckets as f64).floor() as usize;
    for bucket in (0..buckets).rev() {
        let width = LOW.pow(bucket);
        let size = share / most_listed(bucket);
        let mut spare = 0usize;
        let mut start = bucket_end(now, bucket + 1);
        while start < bucket_end(now, bucket) {
            let space = size + spare;
            let piece = (start * PIECE_NS, (start + width) * PIECE_NS);
            let spent = fill_piece(
                spans,
                costs,
                eligible,
                earliest,
                piece,
                space,
                &mut cut.cover,
            );
            spare = space - spent;
            cut.used += spent;
            start += width;
        }
    }
    cut.finished = cut.cover.len();
    // A piece holds the memories that start in it, and the pieces came oldest
    // first, so this sorts inside each piece: start, then end, then id.
    cut.cover.sort_by_key(|&i| spans[i]);
    let mut open: Vec<usize> = (0..spans.len())
        .filter(|&i| eligible[i] && costs[i] > 0 && spans[i].0 >= now * PIECE_NS)
        .collect();
    open.sort_by_key(|&i| spans[i]);
    let mut kept: usize = open.iter().map(|&i| costs[i]).sum();
    let mut dropped = 0usize;
    while kept > budget - cut.used {
        kept -= costs[open[dropped]];
        dropped += 1;
    }
    cut.cover.extend_from_slice(&open[dropped..]);
    cut.used += kept;
    cut
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: i128 = 3600 * 1_000_000_000;
    const DAY: i128 = 24 * H;
    /// Far from zero so the absolute clock is exercised, aligned to nothing.
    const BASE: i128 = 20_000 * DAY + 7 * H + 123_456_789;

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

    /// Costs that vary with the memory, so the fit test is exercised.
    fn costs(spans: &[(i128, i128, Id)]) -> Vec<usize> {
        spans
            .iter()
            .map(|span| 120 + ((span.0 - BASE).div_euclid(H) % 7) as usize * 40)
            .collect()
    }

    fn cut_at(spans: &[(i128, i128, Id)], budget: usize) -> RecollectionCut {
        select_recollection_cut(spans, &costs(spans), &vec![true; spans.len()], budget)
    }

    /// The ids a cut holds, in emitted order: all of them, or its finished part.
    fn ids(spans: &[(i128, i128, Id)], cut: &RecollectionCut) -> Vec<Id> {
        cut.cover.iter().map(|&i| spans[i].2).collect()
    }
    fn finished(spans: &[(i128, i128, Id)], cut: &RecollectionCut) -> Vec<Id> {
        ids(spans, cut)[..cut.finished].to_vec()
    }

    /// The open piece of a journal.
    fn open_piece(spans: &[(i128, i128, Id)]) -> i128 {
        spans
            .iter()
            .map(|s| s.1)
            .max()
            .unwrap()
            .div_euclid(PIECE_NS)
    }

    /// The pieces `bucket` lists when piece `now` is open, as ranges of bottom
    /// pieces, oldest first.
    fn listed(now: i128, bucket: u32) -> Vec<(i128, i128)> {
        let width = LOW.pow(bucket);
        let (from, to) = (bucket_end(now, bucket + 1), bucket_end(now, bucket));
        assert_eq!((to - from).rem_euclid(width), 0);
        (0..(to - from) / width)
            .map(|k| (from + k * width, from + (k + 1) * width))
            .collect()
    }

    /// The bottom bucket just before, at and just after a fold, and the bucket
    /// above it, which takes in the oldest four as one piece.
    #[test]
    fn a_bucket_folds_its_oldest_four_at_the_eighth() {
        let spans = |from: i128, n: i128, width: i128| -> Vec<(i128, i128)> {
            (0..n)
                .map(|k| (from + k * width, from + (k + 1) * width))
                .collect()
        };
        // Piece 1007 is open: the bottom lists its seven, 1000 to 1006.
        assert_eq!(listed(1007, 0), spans(1000, 7, 1));
        assert_eq!(listed(1007, 1), spans(976, 6, 4));
        // It finishes, and the bottom would hold eight. The oldest four, 1000
        // to 1003, are one 4-wide piece of the bucket above now.
        assert_eq!(listed(1008, 0), spans(1004, 4, 1));
        assert_eq!(listed(1008, 1), spans(976 + 16, 3, 4));
        // One later the bottom lists five again.
        assert_eq!(listed(1009, 0), spans(1004, 5, 1));
        assert_eq!(listed(1009, 1), spans(976 + 16, 3, 4));
        // At 1008 the bucket above would hold its eighth too: six listed, the
        // new one, and 1004 to 1007, which still lie in the bottom as their
        // four parts. So its oldest four, 976 to 991, fold in the same step.
        assert_eq!(*listed(1007, 2).last().unwrap(), (960, 976));
        assert_eq!(*listed(1008, 2).last().unwrap(), (976, 992));
    }

    /// Every bucket holds `LOW` to `HIGH - 1` pieces at every moment, counting
    /// above the bottom the one that still lies in the bucket below. The
    /// buckets meet without a gap, and no bucket lists more than it has room
    /// for.
    #[test]
    fn every_bucket_stays_between_its_marks() {
        let from = BASE.div_euclid(PIECE_NS);
        for now in from..from + 4096 {
            for bucket in 0..6u32 {
                let pieces = listed(now, bucket);
                let held = pieces.len() as i128 + i128::from(bucket > 0);
                assert!(
                    (LOW..HIGH).contains(&held),
                    "bucket {bucket} at {now}: {held}"
                );
                assert!(pieces.len() <= most_listed(bucket));
                assert_eq!(pieces[0].0, bucket_end(now, bucket + 1));
            }
        }
    }

    /// The days before now are never left to one wide piece: each of the last
    /// four finished pieces holds something, at every hour.
    #[test]
    fn the_newest_pieces_always_hold_something() {
        for hours in (40 * 24..70 * 24).step_by(5) {
            let spans = journal(hours, 35);
            let cut = cut_at(&spans, 60_000);
            assert!(cut.used <= 60_000);
            let now = open_piece(&spans);
            for piece in now - LOW..now {
                assert!(
                    cut.cover
                        .iter()
                        .any(|&i| spans[i].0.div_euclid(PIECE_NS) == piece),
                    "nothing from piece {} at hour {hours}",
                    piece - now
                );
            }
        }
    }

    /// A piece that finishes is appended. Every entry before it stays where it
    /// was, unless a bucket folds, and then every entry older than the folded
    /// pieces still does. Dividing a bucket's share over the pieces it holds
    /// fails this, and so does handing a leftover to an older piece.
    #[test]
    fn a_finished_piece_moves_no_older_entry() {
        let mut previous: Option<(i128, RecollectionCut, Vec<Id>, Vec<(i128, i128, Id)>)> = None;
        let (mut appended, mut folded) = (0, 0);
        for hours in 40 * 24..70 * 24 {
            let spans = journal(hours, 35);
            let cut = cut_at(&spans, 60_000);
            let (now, done) = (open_piece(&spans), finished(&spans, &cut));
            if let Some((was_now, _, was, was_spans)) = previous.take() {
                // Where the finest fold between the two moments reaches.
                let kept = if now == was_now {
                    now
                } else {
                    let widest = (1..6)
                        .filter(|&b| now.rem_euclid(LOW.pow(b)) == 0)
                        .max()
                        .unwrap_or(0);
                    bucket_end(was_now, widest + 1).min(bucket_end(now, widest + 1))
                };
                let older = |spans: &[(i128, i128, Id)], ids: &[Id]| -> usize {
                    ids.iter()
                        .take_while(|id| {
                            let span = spans.iter().find(|s| s.2 == **id).unwrap();
                            span.0.div_euclid(PIECE_NS) < kept
                        })
                        .count()
                };
                let stay = if now == was_now || now.rem_euclid(LOW) != 0 {
                    appended += usize::from(now != was_now);
                    was.len()
                } else {
                    let stay = older(&was_spans, &was);
                    folded += usize::from(stay > 0);
                    stay
                };
                assert!(
                    done.len() >= stay && done[..stay] == was[..stay],
                    "an older entry moved at hour {hours}"
                );
            }
            previous = Some((now, cut, done, spans));
        }
        assert!(appended > 20 && folded > 5, "{appended} {folded}");
    }

    /// A leftover goes to the next newer piece of its own bucket and no
    /// further: emptying a newer piece, or an older bucket, changes no older
    /// piece and no other bucket.
    #[test]
    fn a_leftover_goes_only_to_the_next_newer_piece() {
        let spans = journal(60 * 24, 0);
        let now = open_piece(&spans);
        let all = cut_at(&spans, 60_000);
        let without = |from: i128, to: i128| {
            let eligible: Vec<bool> = spans
                .iter()
                .map(|s| !(from..to).contains(&s.0.div_euclid(PIECE_NS)))
                .collect();
            let cut = select_recollection_cut(&spans, &costs(&spans), &eligible, 60_000);
            (cut, eligible)
        };
        let in_range = |cut: &RecollectionCut, from: i128, to: i128| -> Vec<Id> {
            cut.cover
                .iter()
                .filter(|&&i| (from..to).contains(&spans[i].0.div_euclid(PIECE_NS)))
                .map(|&i| spans[i].2)
                .collect()
        };
        let first = open_piece(&spans[..1]);
        // Emptying the newest piece of the bottom bucket changes nothing older.
        let (cut, _) = without(now - 1, now);
        assert_eq!(
            in_range(&cut, first, now - 1),
            in_range(&all, first, now - 1)
        );
        // Emptying the second bucket's oldest piece reaches its own newer
        // pieces only: the bottom bucket is as it was.
        let (from, to) = listed(now, 1)[0];
        let (cut, _) = without(from, to);
        assert_eq!(
            in_range(&cut, bucket_end(now, 1), now),
            in_range(&all, bucket_end(now, 1), now)
        );
        assert_ne!(
            in_range(&cut, to, bucket_end(now, 1)),
            in_range(&all, to, bucket_end(now, 1)),
            "the leftover is used"
        );
    }

    /// Nothing is spent on the time before the first memory: a piece that
    /// began long before it spreads its picks over the part that was lived.
    #[test]
    fn a_piece_spreads_over_its_lived_time_only() {
        let width = 64 * PIECE_NS;
        let begin = (BASE.div_euclid(4 * width) + 1) * 4 * width;
        let start = begin + 3 * width / 4;
        let hours = ((begin + 2 * width + 5 * H) - start) / H;
        let spans: Vec<_> = (0..hours)
            .map(|h| (start + h * H, start + (h + 1) * H, id(h as u128 + 1)))
            .collect();
        let cut = select_recollection_cut(
            &spans,
            &vec![150; spans.len()],
            &vec![true; spans.len()],
            80_000,
        );
        let end = begin + width;
        let picks: Vec<usize> = cut
            .cover
            .iter()
            .copied()
            .filter(|&i| spans[i].0 < end)
            .collect();
        assert!(picks.len() >= 8, "{} picks", picks.len());
        let early = picks
            .iter()
            .filter(|&&i| spans[i].0 - start < (end - start) / 2)
            .count() as f64
            / picks.len() as f64;
        assert!(early <= 0.6, "early fraction {early}");
    }

    /// The open piece holds exactly the memories that start in it, in time
    /// order, and grows at its end alone.
    #[test]
    fn the_open_piece_is_append_only() {
        let mut previous: Option<(i128, Vec<Id>)> = None;
        let mut grew = 0;
        for hours in 50 * 24..52 * 24 {
            let spans = journal(hours, 45);
            let cut = cut_at(&spans, 150_000);
            let now = open_piece(&spans);
            let open = &cut.cover[cut.finished..];
            assert!(open.iter().all(|&i| spans[i].0.div_euclid(PIECE_NS) == now));
            assert!(open.windows(2).all(|w| spans[w[0]].0 < spans[w[1]].0));
            let all = ids(&spans, &cut);
            if let Some((was_now, was)) = previous.take() {
                if was_now == now {
                    assert!(all.starts_with(&was), "cover rewritten at hour {hours}");
                    grew += 1;
                }
            }
            previous = Some((now, all));
        }
        assert!(grew > 10, "{grew}");
    }

    /// When its room is too small the open piece drops its OLDEST memories.
    #[test]
    fn the_open_piece_drops_its_oldest_first() {
        let start = (BASE.div_euclid(PIECE_NS) + 1) * PIECE_NS;
        let spans: Vec<_> = (0..20)
            .map(|k| {
                (
                    start + k * H / 2,
                    start + (k + 1) * H / 2,
                    id(k as u128 + 1),
                )
            })
            .collect();
        let costs = vec![100; spans.len()];
        let mut eligible = vec![true; spans.len()];
        let cut = select_recollection_cut(&spans, &costs, &eligible, 1_050);
        assert_eq!((cut.finished, cut.used), (0, 1_000));
        assert_eq!(cut.cover, (10..20).collect::<Vec<_>>());
        // An ineligible memory is never shown and frees its room.
        eligible[15] = false;
        let cut = select_recollection_cut(&spans, &costs, &eligible, 1_050);
        assert_eq!(cut.cover, [9, 10, 11, 12, 13, 14, 16, 17, 18, 19]);
    }

    /// The cover is read as a life is told: start, then end, then id, the
    /// finished pieces and the open one alike.
    #[test]
    fn the_cover_is_in_time_order() {
        let spans = journal(60 * 24 + 5, 60);
        let cut = cut_at(&spans, 60_000);
        assert!(cut.finished > 20 && cut.finished < cut.cover.len());
        assert!(cut.cover.windows(2).all(|w| spans[w[0]] < spans[w[1]]));
    }

    /// A journal may reach across the clock's origin (signed TAI before its
    /// epoch): the buckets end, and both sides are kept.
    #[test]
    fn a_journal_across_the_clock_origin_keeps_both_sides() {
        let spans = vec![(-PIECE_NS, 0, id(1)), (0, PIECE_NS, id(2))];
        let cut = select_recollection_cut(&spans, &[100, 100], &[true, true], 10_000);
        let mut picked = cut.cover.clone();
        picked.sort();
        assert_eq!(picked, [0, 1]);
        assert!(cut.used <= 10_000);
    }

    /// The order the candidates come in, and so the order memories were
    /// written in, changes nothing.
    #[test]
    fn the_cover_ignores_candidate_input_order() {
        let spans = journal(20 * 24, 18);
        let cut = cut_at(&spans, 20_000);
        assert!(cut.cover.len() < spans.len(), "a recollection is lossy");
        let mut reversed = spans.clone();
        reversed.reverse();
        assert_eq!(
            ids(&reversed, &cut_at(&reversed, 20_000)),
            ids(&spans, &cut)
        );
    }

    /// A candidate's charged length names one ideal stretch of time: it is
    /// taken over a shifted one of the same width and a narrow one of the same
    /// middle. The first best memory that doesn't fit ends its piece.
    #[test]
    fn a_candidates_length_names_its_ideal_stretch() {
        let piece = (0, 4 * PIECE_NS);
        let stretch = piece.1 / 10;
        let spans = vec![
            (0, stretch, id(3)),
            (stretch / 4, stretch + stretch / 4, id(1)),
            (stretch / 4, stretch * 3 / 4, id(2)),
        ];
        let fill = |space| {
            let mut cover = Vec::new();
            let used = fill_piece(&spans, &[10; 3], &[true; 3], 0, piece, space, &mut cover);
            (cover, used)
        };
        assert_eq!(fill(100).0.first(), Some(&0));
        assert_eq!(fill(9), (vec![], 0));
        let cut = select_recollection_cut(&spans[..1], &[11], &[true], 10);
        assert_eq!((cut.cover.len(), cut.used), (0, 0));
    }
}
