//! EXPERIMENTAL sealed-segment recollection (lane cover-20261108-sealed).
//!
//! Not wired into `memory context`, `orient wake` or the resident. It exists
//! so `memory churn-sealed` can replay it beside the greedy walk in use and
//! measure whether a sealed older segment buys prefix reuse.
//!
//! The rule, in plain words. The life is split at a boundary `B` on an
//! absolute grid of period `P`. `P` is the largest power-of-two multiple of
//! the 2^14 s replay unit with `P * ratio <= LIFE`, so the grid gets coarser
//! as the life grows (it doubles each time the life doubles). `B` is the grid
//! point one period behind the newest one at or before the newest memory end:
//! `B = floor(L / P) * P - P`, so the young suffix is always between one and
//! two periods long. `B` therefore moves once per `P` of lived time (a carry),
//! and jumps back when `P` doubles.
//!
//! The older part, everything that starts before `B`, is the SEALED segment.
//! It is the ordinary greedy walk run with the gradient frozen at the moment
//! the seal was struck (`L_s = B + P`, the grid point whose crossing set `B`),
//! over only the memories already observed when the journal first reached
//! `L_s` (`created_at`, the first observation whose running maximum memory end
//! reaches `L_s`), and it stops where that frozen gradient reaches `B`. So a
//! summary written later about an old stretch waits for the next carry
//! instead of rewriting the sealed text the moment it appears.
//!
//! The younger suffix is today's greedy walk with a gradient over `(B, L]`
//! and the rest of the budget, over every memory the seal did not take, so
//! it also absorbs any later-written arc as a fallback.
//!
//! Every quantity above is derived from the spans, their first observation
//! time and the budget: no previous cover, no wall clock. The carry ratio is a
//! declared constant parameter of the experiment.

use super::*;

/// One recollection split into a sealed older segment and a younger suffix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SealedCut {
    /// Selected classes in emission order: the sealed entries first.
    pub cover: Vec<usize>,
    /// How many leading entries of `cover` are sealed.
    pub sealed: usize,
    pub sealed_used: usize,
    pub used: usize,
    /// The seal boundary `B` (absolute TAI key); `None` when the life is too
    /// short for one sealed period.
    pub boundary: Option<i128>,
    pub period: i128,
    /// The observation cutoff the sealed segment was computed under.
    pub strike: Option<i128>,
}

/// The grid unit of the seal: the 2^14 s replay unit.
pub const SEAL_UNIT_NS: i128 = REPLAY_QUANTUM_NS;

/// Largest `SEAL_UNIT_NS * 2^k` with `P * ratio <= life` (at least one unit).
pub fn seal_period(life: i128, ratio: i128) -> i128 {
    let ratio = ratio.max(1);
    let mut period = SEAL_UNIT_NS;
    while period
        .checked_mul(2)
        .and_then(|p| p.checked_mul(ratio))
        .is_some_and(|need| need <= life)
    {
        period *= 2;
    }
    period
}

/// The greedy walk of [`select_recollection_cut`], factored over an explicit
/// gradient origin, cursor cap and candidate mask so both segments use the
/// identical scoring. Marks picks in `selected`.
fn walk(
    spans: &[(i128, i128, Id)],
    costs: &[usize],
    candidate: &[bool],
    selected: &mut [bool],
    origin: i128,
    gradient: &DensityGradient,
    cap: usize,
) -> (Vec<usize>, usize) {
    let mut cover = Vec::new();
    let mut used = 0usize;
    while used < cap {
        let ideal_start = gradient.sample(used).time_ns;
        let mut best: Option<(f64, Id, usize)> = None;
        for (i, &(start, end, id)) in spans.iter().enumerate() {
            let cost = costs[i];
            if selected[i] || !candidate[i] || cost == 0 {
                continue;
            }
            let width = end.saturating_sub(start).max(MOMENT_NS) as f64;
            let actual_start = start.saturating_sub(origin) as f64;
            let actual_end = actual_start + width;
            let ideal_end = gradient.sample(used.saturating_add(cost)).time_ns;
            let start_error = actual_start - ideal_start;
            let end_error = actual_end - ideal_end;
            let score = start_error.mul_add(start_error, end_error * end_error);
            let cand = (score, id, i);
            if best
                .as_ref()
                .is_none_or(|cur| cand.0.total_cmp(&cur.0).then(cand.1.cmp(&cur.1)).is_lt())
            {
                best = Some(cand);
            }
        }
        let Some((_, _, pick)) = best else { break };
        let next = used.saturating_add(costs[pick]);
        if next > cap {
            break;
        }
        selected[pick] = true;
        cover.push(pick);
        used = next;
    }
    (cover, used)
}

/// The sealed-segment recollection. `observed` is each class's first
/// observation time. `cutoff_shift` moves the sealed segment's observation
/// cutoff earlier (zero for the real selector); the replay uses it to ask what
/// the seal would have been if the last moments before the strike had
/// arrived late.
pub fn select_sealed_recollection_cut(
    spans: &[(i128, i128, Id)],
    costs: &[usize],
    observed: &[i128],
    budget: usize,
    ratio: i128,
    cutoff_shift: i128,
) -> SealedCut {
    assert_eq!(spans.len(), costs.len());
    assert_eq!(spans.len(), observed.len());
    let empty = SealedCut {
        cover: Vec::new(),
        sealed: 0,
        sealed_used: 0,
        used: 0,
        boundary: None,
        period: 0,
        strike: None,
    };
    let (Some(earliest), Some(latest)) = (
        spans.iter().map(|s| s.0).min(),
        spans.iter().map(|s| s.1).max(),
    ) else {
        return empty;
    };
    if budget == 0 {
        return empty;
    }
    let life = latest.saturating_sub(earliest);
    let period = seal_period(life, ratio);
    let boundary = latest.div_euclid(period) * period - period;
    let mut selected = vec![false; spans.len()];

    let mut sealed_cover = Vec::new();
    let mut sealed_used = 0usize;
    let mut strike = None;
    if boundary > earliest {
        let struck_at = boundary + period;
        // The first observation at which the journal's lived edge reached the
        // strike point. Total order: observation time, then the class's id.
        let mut order: Vec<usize> = (0..spans.len()).collect();
        order.sort_by(|&a, &b| {
            observed[a]
                .cmp(&observed[b])
                .then(spans[a].2.cmp(&spans[b].2))
        });
        let mut edge = i128::MIN;
        for &i in &order {
            edge = edge.max(spans[i].1);
            if edge >= struck_at {
                strike = Some(observed[i]);
                break;
            }
        }
        let cutoff = strike
            .expect("latest >= struck_at")
            .saturating_sub(cutoff_shift);
        let frozen = DensityGradient::new(struck_at - earliest, budget);
        // Cursor at which the frozen gradient reaches B: its age there is P.
        let behind = (period as f64 / MOMENT_NS as f64).ln_1p() / frozen.rate;
        let cap = (budget as f64 - behind).floor().max(0.0) as usize;
        let candidate: Vec<bool> = spans
            .iter()
            .zip(observed)
            .map(|(s, &o)| s.0 < boundary && o <= cutoff)
            .collect();
        let (cover, used) = walk(
            spans,
            costs,
            &candidate,
            &mut selected,
            earliest,
            &frozen,
            cap,
        );
        sealed_cover = cover;
        sealed_used = used;
    }

    let (origin, suffix_life) = if strike.is_some() {
        (boundary, latest - boundary)
    } else {
        (earliest, life)
    };
    let rest = budget - sealed_used;
    let gradient = DensityGradient::new(suffix_life, rest);
    let candidate = vec![true; spans.len()];
    let (suffix, suffix_used) = walk(
        spans,
        costs,
        &candidate,
        &mut selected,
        origin,
        &gradient,
        rest,
    );

    let sealed = sealed_cover.len();
    let mut cover = sealed_cover;
    cover.extend(suffix);
    SealedCut {
        cover,
        sealed,
        sealed_used,
        used: sealed_used + suffix_used,
        boundary: strike.map(|_| boundary),
        period,
        strike,
    }
}

/// Characters of the longest common entry prefix.
fn lcp_chars(a: &[usize], b: &[usize], costs: &[usize]) -> usize {
    a.iter()
        .zip(b)
        .take_while(|(x, y)| x == y)
        .map(|(x, _)| costs[*x])
        .sum()
}

#[derive(Default)]
struct Tally {
    rereads_lcp: Vec<usize>,
    rereads_saved: Vec<usize>,
    full_saved: usize,
    fills: Vec<f64>,
    silent_steps: usize,
}

impl Tally {
    fn line(&self, label: &str) -> String {
        let stat = |v: &[usize]| {
            let mut v = v.to_vec();
            v.sort_unstable();
            (
                v.get(v.len() / 2).copied().unwrap_or(0),
                v.last().copied().unwrap_or(0),
                v.iter().sum::<usize>(),
            )
        };
        let (lm, lx, lt) = stat(&self.rereads_lcp);
        let (sm, sx, st) = stat(&self.rereads_saved);
        let mean_fill = self.fills.iter().sum::<f64>() / self.fills.len().max(1) as f64;
        let min_fill = self.fills.iter().copied().fold(f64::INFINITY, f64::min);
        format!(
            "{label}: mean fill {mean_fill:.1}% (min {min_fill:.1}%); \
             reread from first changed entry: median {lm}, max {lx}, total {lt}; \
             reread with one saved prefix: median {sm}, max {sx}, total {st}, \
             {} of {} transitions reread everything; {} step(s) with a silent stretch > LIFE/4",
            self.full_saved,
            self.rereads_saved.len(),
            self.silent_steps
        )
    }
}

fn fmt_key(key: i128) -> String {
    fmt_epoch(key_to_epoch(key))
}

/// Replay the greedy walk and the sealed selector (one per ratio) over the
/// journal's observation history, as [`replay_cover`] does, and render the
/// comparison as lines.
pub fn replay_compare<B: BlobStoreGet, P: TriblePattern>(
    space: &P,
    ws: &B,
    budget: usize,
    steps: usize,
    step_units: i128,
    ratios: &[i128],
) -> Result<Vec<String>> {
    let raw_spans = collect_chunk_spans(space);
    let (spans, classes) = recollection_classes(&raw_spans);
    let (observed_at, unobserved) = recollection_class_observations(space, &raw_spans, &classes);
    let mut lines = Vec::new();
    let Some(latest_observation) = observed_at.iter().copied().max() else {
        lines.push("no memory chunks".into());
        return Ok(lines);
    };
    let mut raw_costs: Vec<Option<usize>> = vec![None; raw_spans.len()];
    let mut class_costs: Vec<Option<usize>> = vec![None; spans.len()];
    let mut costs = Vec::with_capacity(spans.len());
    for i in 0..spans.len() {
        costs.push(recollection_class_cost(
            ws,
            space,
            &raw_spans,
            &mut raw_costs,
            &classes,
            &mut class_costs,
            i,
            false,
        )?);
    }
    let step_ns = step_units.max(1) * REPLAY_QUANTUM_NS;
    let pct = |x: usize| 100.0 * x as f64 / budget.max(1) as f64;

    // Per step: the visible classes and both selections, global indices.
    struct Step {
        point: i128,
        edge: i128,
        base: Vec<usize>,
        base_used: usize,
        base_silent: bool,
        sealed: Vec<SealedCut>,
        sealed_silent: Vec<bool>,
    }
    let mut steps_out: Vec<Step> = Vec::new();
    let mut seals: Vec<Vec<(i128, i128, Vec<usize>)>> = vec![Vec::new(); ratios.len()];
    let first_point = latest_observation - (steps as i128) * step_ns;
    for k in (0..=steps).rev() {
        let point = latest_observation - (k as i128) * step_ns;
        let map: Vec<usize> = (0..spans.len())
            .filter(|&i| observed_at[i] <= point)
            .collect();
        if map.is_empty() {
            continue;
        }
        let sub: Vec<_> = map.iter().map(|&i| spans[i]).collect();
        let sub_costs: Vec<_> = map.iter().map(|&i| costs[i]).collect();
        let sub_obs: Vec<_> = map.iter().map(|&i| observed_at[i]).collect();
        let edge = sub.iter().map(|s| s.1).max().unwrap_or(0);
        let base = select_recollection_cut(&sub, &sub_costs, &vec![true; sub.len()], budget);
        let base_silent = !silent_life_quarters(&sub, &base.cover).is_empty();
        let mut sealed = Vec::new();
        let mut sealed_silent = Vec::new();
        for (r, &ratio) in ratios.iter().enumerate() {
            let mut cut =
                select_sealed_recollection_cut(&sub, &sub_costs, &sub_obs, budget, ratio, 0);
            sealed_silent.push(!silent_life_quarters(&sub, &cut.cover).is_empty());
            // Late-arrival probes, once per distinct seal.
            if let (Some(b), Some(c)) = (cut.boundary, cut.strike) {
                if !seals[r].iter().any(|(sb, sc, _)| *sb == b && *sc == c) {
                    let mut shifted = Vec::new();
                    for lag_h in [1i128, 6, 24] {
                        let lag = lag_h * 3600 * 1_000_000_000;
                        let late = select_sealed_recollection_cut(
                            &sub, &sub_costs, &sub_obs, budget, ratio, lag,
                        );
                        let same = late.cover[..late.sealed] == cut.cover[..cut.sealed];
                        shifted.push(if same { 0 } else { 1 });
                    }
                    seals[r].push((b, c, shifted));
                }
            }
            for x in cut.cover.iter_mut() {
                *x = map[*x];
            }
            sealed.push(cut);
        }
        steps_out.push(Step {
            point,
            edge,
            base: base.cover.iter().map(|&j| map[j]).collect(),
            base_used: base.used,
            base_silent,
            sealed,
            sealed_silent,
        });
    }

    lines.push(format!(
        "frame: budget {budget} chars; {} step(s) of {step_units} x 2^14 s from observation {} to {}; \
         {} exact-span classes ({} with range-end fallback for missing created_at); \
         reread counts rendered characters of the cover only (no beliefs, goals or live turns)",
        steps_out.len(),
        fmt_key(first_point.max(steps_out.first().map_or(first_point, |s| s.point))),
        fmt_key(latest_observation),
        spans.len(),
        unobserved,
    ));

    // Baseline.
    lines.push(String::new());
    lines.push(
        "== greedy walk in use (select_recollection_cut); saved prefix = every recall".into(),
    );
    lines.push(format!(
        "{:<20} {:<20} {:>5} {:>8} {:>6} {:>8} {:>8}",
        "observed", "memory edge", "n", "used", "fill%", "lcp-rr", "saved-rr"
    ));
    let mut base_tally = Tally::default();
    for (k, s) in steps_out.iter().enumerate() {
        let (lcp_rr, saved_rr) = if k == 0 {
            (0, 0)
        } else {
            let prev = &steps_out[k - 1];
            let lcp = lcp_chars(&s.base, &prev.base, &costs);
            let reuse = if s.base.starts_with(&prev.base) {
                prev.base_used
            } else {
                0
            };
            (
                s.base_used - lcp.min(s.base_used),
                s.base_used - reuse.min(s.base_used),
            )
        };
        if k > 0 {
            base_tally.rereads_lcp.push(lcp_rr);
            base_tally.rereads_saved.push(saved_rr);
            if saved_rr == s.base_used {
                base_tally.full_saved += 1;
            }
        }
        base_tally.fills.push(pct(s.base_used));
        if s.base_silent {
            base_tally.silent_steps += 1;
        }
        lines.push(format!(
            "{:<20} {:<20} {:>5} {:>8} {:>5.1}% {:>8} {:>8}{}",
            fmt_key(s.point),
            fmt_key(s.edge),
            s.base.len(),
            s.base_used,
            pct(s.base_used),
            lcp_rr,
            saved_rr,
            if s.base_silent { "  SILENT>LIFE/4" } else { "" }
        ));
    }
    let base_summary = base_tally.line("greedy walk");

    let mut summaries = vec![base_summary];
    for (r, &ratio) in ratios.iter().enumerate() {
        lines.push(String::new());
        lines.push(format!(
            "== sealed segment, ratio {ratio} (P = largest 2^k x 2^14 s with P x {ratio} <= LIFE); saved prefix = the sealed recalls"
        ));
        lines.push(format!(
            "{:<20} {:>5} {:<20} {:>6} {:>8} {:>6} {:>8} {:>6} {:>8} {:>8} {:>8}  note",
            "observed",
            "P(d)",
            "boundary B",
            "nseal",
            "sealed",
            "seal%",
            "used",
            "fill%",
            "kept",
            "lcp-rr",
            "saved-rr"
        ));
        let mut tally = Tally::default();
        let mut carries = 0usize;
        let mut other_changes = 0usize;
        let mut sealed_share = Vec::new();
        for (k, s) in steps_out.iter().enumerate() {
            let cut = &s.sealed[r];
            let seg = &cut.cover[..cut.sealed];
            let mut note = String::new();
            let (kept, lcp_rr, saved_rr) = if k == 0 {
                (0, 0, 0)
            } else {
                let prev = &steps_out[k - 1].sealed[r];
                let prev_seg = &prev.cover[..prev.sealed];
                let lcp = lcp_chars(&cut.cover, &prev.cover, &costs);
                let reuse = if cut.cover.starts_with(prev_seg) {
                    prev.sealed_used
                } else {
                    0
                };
                if seg != prev_seg {
                    if cut.boundary != prev.boundary {
                        carries += 1;
                        note.push_str("carry");
                    } else {
                        other_changes += 1;
                        note.push_str("SEAL CHANGED without carry");
                    }
                }
                (
                    reuse,
                    cut.used - lcp.min(cut.used),
                    cut.used - reuse.min(cut.used),
                )
            };
            if k > 0 {
                tally.rereads_lcp.push(lcp_rr);
                tally.rereads_saved.push(saved_rr);
                if saved_rr == cut.used {
                    tally.full_saved += 1;
                }
            }
            tally.fills.push(pct(cut.used));
            sealed_share.push(pct(cut.sealed_used));
            if s.sealed_silent[r] {
                tally.silent_steps += 1;
                note.push_str("  SILENT>LIFE/4");
            }
            lines.push(format!(
                "{:<20} {:>5.1} {:<20} {:>6} {:>8} {:>5.1}% {:>8} {:>5.1}% {:>8} {:>8} {:>8}  {note}",
                fmt_key(s.point),
                cut.period as f64 / 86_400e9,
                cut.boundary.map_or("-".into(), fmt_key),
                cut.sealed,
                cut.sealed_used,
                pct(cut.sealed_used),
                cut.used,
                pct(cut.used),
                kept,
                lcp_rr,
                saved_rr,
            ));
        }
        let mean_share = sealed_share.iter().sum::<f64>() / sealed_share.len().max(1) as f64;
        let lag_counts: Vec<String> = [1, 6, 24]
            .iter()
            .enumerate()
            .map(|(j, h)| {
                let n: usize = seals[r].iter().map(|(_, _, v)| v[j]).sum();
                format!("{n}/{} at {h}h", seals[r].len())
            })
            .collect();
        summaries.push(format!(
            "{}; sealed segment mean {mean_share:.1}% of budget; {carries} carry(ies), {other_changes} seal change(s) without carry; \
             late-arrival probe (seal recomputed without the memories observed in the last LAG before its strike) changes the seal for {}",
            tally.line(&format!("sealed ratio {ratio}")),
            lag_counts.join(", ")
        ));
    }
    lines.push(String::new());
    lines.push("== summary".into());
    lines.extend(summaries);
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: i128 = 3600 * 1_000_000_000;

    /// A journal of hourly entries plus daily arcs, each observed one hour
    /// after its end.
    fn journal(days: i128) -> (Vec<(i128, i128, Id)>, Vec<usize>, Vec<i128>) {
        let mut raw = Vec::new();
        for d in 0..days {
            raw.push((d * 24 * H, (d + 1) * 24 * H));
            for h in 0..24 {
                let s = (d * 24 + h) * H;
                raw.push((s, s + H));
            }
        }
        let spans: Vec<_> = raw
            .iter()
            .enumerate()
            .map(|(i, &(s, e))| {
                let id = Id::new(u128::to_be_bytes(
                    0x5E000000000000000000000000000000 + i as u128,
                ))
                .expect("nonzero");
                (s + 1_000 * 24 * H, e + 1_000 * 24 * H, id)
            })
            .collect();
        let costs = spans.iter().map(|_| 200).collect();
        let observed = spans.iter().map(|s| s.1 + H).collect();
        (spans, costs, observed)
    }

    #[test]
    fn seal_period_doubles_with_life() {
        let p = seal_period(1000 * SEAL_UNIT_NS, 32);
        assert!(p * 32 <= 1000 * SEAL_UNIT_NS && p * 64 > 1000 * SEAL_UNIT_NS);
        assert_eq!(seal_period(SEAL_UNIT_NS, 32), SEAL_UNIT_NS);
    }

    #[test]
    fn sealed_segment_survives_appends_until_a_carry() {
        let (spans, costs, observed) = journal(60);
        let budget = 20_000;
        let mut previous: Option<SealedCut> = None;
        let mut carries = 0;
        let mut kept = 0;
        for hours in (40 * 24..60 * 24).step_by(5) {
            let now = 1_000 * 24 * H + hours * H;
            let vis: Vec<usize> = (0..spans.len()).filter(|&i| observed[i] <= now).collect();
            let s: Vec<_> = vis.iter().map(|&i| spans[i]).collect();
            let c: Vec<_> = vis.iter().map(|&i| costs[i]).collect();
            let o: Vec<_> = vis.iter().map(|&i| observed[i]).collect();
            let mut cut = select_sealed_recollection_cut(&s, &c, &o, budget, 8, 0);
            assert!(cut.used <= budget);
            assert!(cut.sealed > 0);
            for x in cut.cover.iter_mut() {
                *x = vis[*x];
            }
            if let Some(prev) = previous.take() {
                let same = cut.cover[..cut.sealed] == prev.cover[..prev.sealed];
                if prev.boundary == cut.boundary {
                    assert!(same, "a seal changed without its boundary moving");
                    assert!(cut.cover.starts_with(&prev.cover[..prev.sealed]));
                    kept += 1;
                } else {
                    carries += 1;
                }
            }
            previous = Some(cut);
        }
        assert!(carries >= 1 && kept > carries);
    }

    #[test]
    fn short_life_has_no_seal_and_is_the_greedy_walk() {
        // Four hourly entries: the life is shorter than one seal unit, so no
        // grid point one period behind the newest can lie inside it.
        let (spans, costs, observed) = journal(1);
        let (spans, costs, observed) = (&spans[1..5], &costs[1..5], &observed[1..5]);
        let cut = select_sealed_recollection_cut(spans, costs, observed, 5_000, 32, 0);
        assert_eq!(cut.boundary, None);
        let base = select_recollection_cut(spans, costs, &vec![true; spans.len()], 5_000);
        assert_eq!(cut.cover, base.cover);
    }
}
