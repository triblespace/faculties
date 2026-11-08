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

// ---------------------------------------------------------------------------
// quantized anchor: today's sampler with LIFE and "now" stepped on a grid
// ---------------------------------------------------------------------------

/// Today's walk with its time map computed from a quantized anchor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnchoredCut {
    pub cover: Vec<usize>,
    /// Leading entries chosen against the quantized (anchored) targets.
    pub anchored: usize,
    pub anchored_used: usize,
    pub used: usize,
    /// The anchor: quantized LIFE and the snapped "now" (absolute key).
    pub life_q: i128,
    pub now_q: i128,
    pub grid: i128,
}

/// The walk in use, but its targets `T(s)` come from a QUANTIZED anchor:
/// `LIFE_q` is the true life rounded UP to `2^k` units of 2^14 s (it steps
/// only when the life doubles), the grid is `G = max(unit, LIFE_q / 2^c)`, and
/// the virtual now is `N_q + G` with `N_q = floor(L / G) * G`, the grid
/// boundary at or before the newest memory end. Positions whose target lies
/// before `N_q` (cursor up to where the anchored age equals `G`) are filled by
/// the unchanged greedy walk against those fixed targets, over every memory.
/// The young region `(N_q, L]` then gets the remaining budget, again with
/// today's walk and a gradient that tracks the true now. Pure in the pile and
/// budget, with `c` a declared constant; there is no candidate freeze, so a
/// memory written later about an old stretch is eligible immediately.
pub fn select_anchored_recollection_cut(
    spans: &[(i128, i128, Id)],
    costs: &[usize],
    budget: usize,
    c: u32,
) -> AnchoredCut {
    assert_eq!(spans.len(), costs.len());
    let mut out = AnchoredCut {
        cover: Vec::new(),
        anchored: 0,
        anchored_used: 0,
        used: 0,
        life_q: 0,
        now_q: 0,
        grid: 0,
    };
    let (Some(earliest), Some(latest)) = (
        spans.iter().map(|s| s.0).min(),
        spans.iter().map(|s| s.1).max(),
    ) else {
        return out;
    };
    if budget == 0 {
        return out;
    }
    let unit = SEAL_UNIT_NS;
    let life = latest.saturating_sub(earliest).max(1);
    let mut life_q = unit;
    while life_q < life {
        life_q *= 2;
    }
    let grid = (life_q >> c).max(unit);
    let now_q = latest.div_euclid(grid) * grid;
    out.life_q = life_q;
    out.now_q = now_q;
    out.grid = grid;
    let mut selected = vec![false; spans.len()];
    let candidate = vec![true; spans.len()];
    let gradient = DensityGradient::new(life_q, budget);
    let origin = now_q + grid - life_q;
    let behind = (grid as f64 / MOMENT_NS as f64).ln_1p() / gradient.rate;
    let cap = (budget as f64 - behind).floor().max(0.0) as usize;
    let (head, head_used) = walk(
        spans,
        costs,
        &candidate,
        &mut selected,
        origin,
        &gradient,
        cap,
    );
    out.anchored = head.len();
    out.anchored_used = head_used;
    out.cover = head;
    let rest = budget - head_used;
    let young = DensityGradient::new(latest - now_q, rest);
    let (tail, tail_used) = walk(spans, costs, &candidate, &mut selected, now_q, &young, rest);
    out.cover.extend(tail);
    out.used = head_used + tail_used;
    out
}

/// Replay the walk in use beside the quantized-anchor walk for each `c`.
pub fn replay_anchored<B: BlobStoreGet, P: TriblePattern>(
    space: &P,
    ws: &B,
    budget: usize,
    steps: usize,
    step_units: i128,
    cs: &[u32],
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
    struct Step {
        point: i128,
        base: Vec<usize>,
        base_used: usize,
        base_silent: bool,
        cuts: Vec<AnchoredCut>,
        silent: Vec<bool>,
    }
    let mut out_steps = Vec::new();
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
        let base = select_recollection_cut(&sub, &sub_costs, &vec![true; sub.len()], budget);
        let base_silent = !silent_life_quarters(&sub, &base.cover).is_empty();
        let mut cuts = Vec::new();
        let mut silent = Vec::new();
        for &c in cs {
            let mut cut = select_anchored_recollection_cut(&sub, &sub_costs, budget, c);
            silent.push(!silent_life_quarters(&sub, &cut.cover).is_empty());
            for x in cut.cover.iter_mut() {
                *x = map[*x];
            }
            cuts.push(cut);
        }
        out_steps.push(Step {
            point,
            base: base.cover.iter().map(|&j| map[j]).collect(),
            base_used: base.used,
            base_silent,
            cuts,
            silent,
        });
    }
    lines.push(format!(
        "frame: budget {budget} CHARACTERS; {} replay points ({} transitions) of {step_units} x 2^14 s, observation (created_at) {} to {}; \
         {} exact-span classes ({} with range-end fallback); all figures are rendered cover characters compared as entry sequences",
        out_steps.len(),
        out_steps.len().saturating_sub(1),
        fmt_key(out_steps.first().map_or(latest_observation, |s| s.point)),
        fmt_key(latest_observation),
        spans.len(),
        unobserved,
    ));
    let changed: Vec<usize> = (1..out_steps.len())
        .filter(|&k| out_steps[k].base != out_steps[k - 1].base)
        .collect();
    let mean = |v: &[usize]| v.iter().sum::<usize>() as f64 / v.len().max(1) as f64;
    let base_used: Vec<usize> = changed.iter().map(|&k| out_steps[k].base_used).collect();
    let base_lcp: Vec<usize> = changed
        .iter()
        .map(|&k| lcp_chars(&out_steps[k].base, &out_steps[k - 1].base, &costs))
        .collect();
    let base_full = (1..out_steps.len())
        .filter(|&k| !out_steps[k].base.starts_with(&out_steps[k - 1].base))
        .count();
    let mut summary = vec![format!(
        "walk in use: {} of {} transitions changed; full rereads with today's whole-cover saved prefix {}; on changed transitions mean cover {:.0}k, \
         mean common entry prefix {:.0}k (reread from first change mean {:.0}k); mean fill {:.1}%; {} step(s) silent > LIFE/4",
        changed.len(),
        out_steps.len().saturating_sub(1),
        base_full,
        mean(&base_used) / 1e3,
        mean(&base_lcp) / 1e3,
        (mean(&base_used) - mean(&base_lcp)) / 1e3,
        out_steps.iter().map(|s| pct(s.base_used)).sum::<f64>() / out_steps.len().max(1) as f64,
        out_steps.iter().filter(|s| s.base_silent).count(),
    )];
    for (ci, &c) in cs.iter().enumerate() {
        lines.push(String::new());
        lines.push(format!(
            "== quantized anchor, c = {c} (grid G = LIFE_q / 2^{c}, at least 2^14 s); head = entries against anchored targets; \
             keep-head = chars reused with ONE prefix saved at the head's end; lcp = common entry prefix chars"
        ));
        lines.push(format!(
            "{:<20} {:>7} {:>9} {:<20} {:>8} {:>8} {:>6} {:>8} {:>8}  note",
            "observed", "G(d)", "LIFE_q(d)", "now_q", "head", "used", "fill%", "keep-hd", "lcp"
        ));
        let mut steps_anchor = 0usize;
        let mut steps_life = 0usize;
        let mut head_full = 0usize;
        let mut whole_full = 0usize;
        let mut head_changed_same_anchor = 0usize;
        let (mut keep_v, mut lcp_v, mut used_v, mut head_v) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for (k, s) in out_steps.iter().enumerate() {
            let cut = &s.cuts[ci];
            let mut note = String::new();
            let (mut keep, mut lcp) = (0, 0);
            if k > 0 {
                let prev = &out_steps[k - 1].cuts[ci];
                let prev_head = &prev.cover[..prev.anchored];
                let head = &cut.cover[..cut.anchored];
                if (prev.life_q, prev.now_q) != (cut.life_q, cut.now_q) {
                    steps_anchor += 1;
                    note.push_str("anchor-step ");
                    if prev.life_q != cut.life_q {
                        steps_life += 1;
                        note.push_str("(LIFE_q doubled) ");
                    }
                }
                if cut.cover.starts_with(prev_head) {
                    keep = prev.anchored_used;
                } else {
                    head_full += 1;
                    if (prev.life_q, prev.now_q) == (cut.life_q, cut.now_q) {
                        head_changed_same_anchor += 1;
                        note.push_str("HEAD CHANGED under the same anchor ");
                    }
                }
                if !cut.cover.starts_with(&prev.cover) {
                    whole_full += 1;
                }
                let _ = head;
                lcp = lcp_chars(&cut.cover, &prev.cover, &costs);
                if changed.contains(&k) {
                    keep_v.push(keep);
                    lcp_v.push(lcp);
                    used_v.push(cut.used);
                    head_v.push(cut.anchored_used);
                }
            }
            if s.silent[ci] {
                note.push_str("SILENT>LIFE/4");
            }
            lines.push(format!(
                "{:<20} {:>7.2} {:>9.1} {:<20} {:>8} {:>8} {:>5.1}% {:>8} {:>8}  {note}",
                fmt_key(s.point),
                cut.grid as f64 / 86_400e9,
                cut.life_q as f64 / 86_400e9,
                fmt_key(cut.now_q),
                cut.anchored_used,
                cut.used,
                pct(cut.used),
                keep,
                lcp,
            ));
        }
        let fills: Vec<f64> = out_steps.iter().map(|s| pct(s.cuts[ci].used)).collect();
        let total = |v: &[usize]| v.iter().sum::<usize>();
        summary.push(format!(
            "anchor c={c}: anchor stepped on {steps_anchor} of {} transitions ({steps_life} of them LIFE_q doubling); mean fill {:.1}% (min {:.1}%); \
             {} step(s) silent > LIFE/4; full rereads with a whole-cover saved prefix {whole_full}, with a prefix saved at the head's end {head_full} \
             (of which {head_changed_same_anchor} with the anchor unchanged); on the {} changed transitions: mean head {:.0}k chars, \
             mean reused at the head's end {:.0}k (reread {:.1}M total), mean common entry prefix {:.0}k (reread from first change mean {:.0}k, {:.1}M total); \
             walk in use: common-prefix reread {:.1}M total",
            out_steps.len().saturating_sub(1),
            fills.iter().sum::<f64>() / fills.len().max(1) as f64,
            fills.iter().copied().fold(f64::INFINITY, f64::min),
            out_steps.iter().filter(|s| s.silent[ci]).count(),
            used_v.len(),
            mean(&head_v) / 1e3,
            mean(&keep_v) / 1e3,
            (total(&used_v) - total(&keep_v)) as f64 / 1e6,
            mean(&lcp_v) / 1e3,
            (mean(&used_v) - mean(&lcp_v)) / 1e3,
            (total(&used_v) - total(&lcp_v)) as f64 / 1e6,
            (total(&base_used) - total(&base_lcp)) as f64 / 1e6,
        ));
    }
    lines.push(String::new());
    lines.push("== summary".into());
    lines.extend(summary);
    Ok(lines)
}

// ---------------------------------------------------------------------------
// calendar-unit grid: years, months, (weeks,) days, hours of today, in UTC
// ---------------------------------------------------------------------------

/// Days since 1970-01-01 of a proleptic Gregorian date (H. Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (
        if m <= 2 {
            yoe + era * 400 + 1
        } else {
            yoe + era * 400
        },
        m,
        d,
    )
}

/// Monday = 0 (1970-01-01 was a Thursday).
fn weekday(z: i64) -> i64 {
    (z + 3).rem_euclid(7)
}

/// TAI key of `hour`:00 UTC on day `z`.
fn utc_key(z: i64, hour: u8) -> i128 {
    let (y, m, d) = civil_from_days(z);
    Epoch::from_gregorian_utc_hms(y as i32, m as u8, d as u8, hour, 0, 0)
        .to_tai_duration()
        .total_nanoseconds()
}

/// UTC day number and hour of a TAI key.
fn utc_day_hour(key: i128) -> (i64, u8) {
    let (y, m, d, h, _, _, _) = key_to_epoch(key).to_gregorian_utc();
    (days_from_civil(y as i64, m as i64, d as i64), h)
}

pub const CAL_HOUR: u32 = 0;
pub const CAL_DAY: u32 = 1;
pub const CAL_WEEK: u32 = 2;
pub const CAL_MONTH: u32 = 3;
pub const CAL_YEAR: u32 = 4;

pub fn cal_level_name(level: u32) -> &'static str {
    ["hour", "day", "week", "month", "year"][level as usize]
}

/// The sampling slots for a newest memory end `latest`, coarsest first, as
/// `(level, start, end)` TAI keys, plus the start of the open hour.
///
/// Complete years before the current one, complete months of the current year
/// before the current month, [complete weeks of the current month before the
/// current week,] complete days before today, complete hours of today before
/// the current hour. With `weeks`, a week belongs to the month (and year) of
/// its Thursday, so months and years begin on the Monday of the week holding
/// their 4th of the month (ISO 8601's rule for years) and everything nests.
/// Without weeks, months and years are the calendar's own.
pub fn calendar_slots(earliest: i128, latest: i128, weeks: bool) -> (Vec<(u32, i128, i128)>, i128) {
    let (z_now, h_now) = utc_day_hour(latest);
    let (z_first, _) = utc_day_hour(earliest);
    let mut slots = Vec::new();
    let monday = |z: i64| z - weekday(z);
    if weeks {
        let w0 = monday(z_now);
        let (y0, m0, _) = civil_from_days(w0 + 3);
        let month_start = |y: i64, m: i64| monday(days_from_civil(y, m, 4));
        let (wy_first, _, _) = civil_from_days(monday(z_first) + 3);
        for y in wy_first..y0 {
            slots.push((
                CAL_YEAR,
                utc_key(month_start(y, 1), 0),
                utc_key(month_start(y + 1, 1), 0),
            ));
        }
        for m in 1..m0 {
            slots.push((
                CAL_MONTH,
                utc_key(month_start(y0, m), 0),
                utc_key(month_start(y0, m + 1), 0),
            ));
        }
        let mut w = month_start(y0, m0);
        while w < w0 {
            slots.push((CAL_WEEK, utc_key(w, 0), utc_key(w + 7, 0)));
            w += 7;
        }
        for z in w0..z_now {
            slots.push((CAL_DAY, utc_key(z, 0), utc_key(z + 1, 0)));
        }
    } else {
        let (y0, m0, d0) = civil_from_days(z_now);
        let (y_first, _, _) = civil_from_days(z_first);
        for y in y_first..y0 {
            slots.push((
                CAL_YEAR,
                utc_key(days_from_civil(y, 1, 1), 0),
                utc_key(days_from_civil(y + 1, 1, 1), 0),
            ));
        }
        for m in 1..m0 {
            slots.push((
                CAL_MONTH,
                utc_key(days_from_civil(y0, m, 1), 0),
                utc_key(days_from_civil(y0, m + 1, 1), 0),
            ));
        }
        for d in 1..d0 {
            let z = days_from_civil(y0, m0, d);
            slots.push((CAL_DAY, utc_key(z, 0), utc_key(z + 1, 0)));
        }
    }
    for h in 0..h_now {
        slots.push((CAL_HOUR, utc_key(z_now, h), utc_key(z_now, h + 1)));
    }
    (slots, utc_key(z_now, h_now))
}

/// One slot of a [`CalendarCut`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CalSlot {
    pub start: i128,
    pub end: i128,
    pub allowance: usize,
    pub entries: usize,
    pub used: usize,
}

/// One calendar level of a [`CalendarCut`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CalLevel {
    pub level: u32,
    pub slots: Vec<CalSlot>,
    pub allowance: usize,
    /// Entries `cover[start..start + len]` belong to this level.
    pub start: usize,
    pub len: usize,
    pub used: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CalendarCut {
    pub cover: Vec<usize>,
    pub levels: Vec<CalLevel>,
    pub years: usize,
    pub open_start: i128,
    pub open_space: usize,
    pub used: usize,
}

impl CalendarCut {
    pub fn level_entries(&self, level: u32) -> &[usize] {
        self.levels
            .iter()
            .find(|l| l.level == level)
            .map_or(&[][..], |l| &self.cover[l.start..l.start + l.len])
    }
    pub fn level_span(&self, level: u32) -> Option<(i128, i128, usize)> {
        self.levels.iter().find(|l| l.level == level).map(|l| {
            (
                l.slots.first().map_or(0, |s| s.start),
                l.slots.last().map_or(0, |s| s.end),
                l.slots.len(),
            )
        })
    }
}

/// The calendar-unit selector. Slots come from [`calendar_slots`]. Each
/// level's share of the budget is the log-age gradient's mass over that
/// level's nominal band (hour 1 h..1 d, day 1 d..1 w or 1 mo, week 1 w..1 mo,
/// month 1 mo..1 y, year 1 y..(years+1) y), normalised over 0..(years+1) y,
/// with anchor `A` (3 s = today's shape, 2^14 s = the 2026-09-05 tiled
/// shape). A level's allowance (share plus what older levels left) is split
/// evenly over its slots, unspent passing on; inside each slot today's greedy
/// walk chooses among memories that START in the slot. With `frozen`, a
/// level's candidates are only those observed by the first observation at
/// which the journal's lived edge reached the end of the level's newest slot
/// (policy a); otherwise every memory is a candidate at once (policy b). The
/// open hour, with whatever is left, is today's walk over everything not yet
/// taken. Pure in the pile, the budget and the declared constants.
pub fn select_calendar_recollection_cut(
    spans: &[(i128, i128, Id)],
    costs: &[usize],
    observed: &[i128],
    budget: usize,
    anchor_ns: i128,
    weeks: bool,
    frozen: bool,
) -> CalendarCut {
    let mut out = CalendarCut {
        cover: Vec::new(),
        levels: Vec::new(),
        years: 0,
        open_start: 0,
        open_space: 0,
        used: 0,
    };
    let (Some(earliest), Some(latest)) = (
        spans.iter().map(|s| s.0).min(),
        spans.iter().map(|s| s.1).max(),
    ) else {
        return out;
    };
    if budget == 0 {
        return out;
    }
    let (slots, open_start) = calendar_slots(earliest, latest, weeks);
    out.open_start = open_start;
    let years = slots.iter().filter(|s| s.0 == CAL_YEAR).count();
    out.years = years;
    const HOUR: f64 = 3600.0;
    const DAY: f64 = 86_400.0;
    const WEEK: f64 = 604_800.0;
    const MONTH: f64 = 2_629_746.0;
    const YEAR: f64 = 31_556_952.0;
    let a = anchor_ns.max(1) as f64 / 1e9;
    let norm = ((a + (years as f64 + 1.0) * YEAR) / a).ln();
    let band = |lo: f64, hi: f64| ((a + hi) / (a + lo)).ln() / norm * budget as f64;
    let share = |level: u32| -> usize {
        (match level {
            CAL_HOUR => band(HOUR, DAY),
            CAL_DAY => band(DAY, if weeks { WEEK } else { MONTH }),
            CAL_WEEK => band(WEEK, MONTH),
            CAL_MONTH => band(MONTH, YEAR),
            _ => band(YEAR, (years as f64 + 1.0) * YEAR),
        })
        .floor() as usize
    };
    let mut order: Vec<usize> = (0..spans.len()).collect();
    order.sort_by(|&x, &y| {
        observed[x]
            .cmp(&observed[y])
            .then(spans[x].2.cmp(&spans[y].2))
    });
    let mut edges: Vec<(i128, i128)> = Vec::with_capacity(order.len());
    let mut edge = i128::MIN;
    for &i in &order {
        edge = edge.max(spans[i].1);
        edges.push((edge, observed[i]));
    }
    let strike = |end: i128| -> i128 {
        let k = edges.partition_point(|&(e, _)| e < end);
        edges.get(k).map_or(i128::MAX, |&(_, o)| o)
    };
    let mut selected = vec![false; spans.len()];
    let mut carry = 0usize;
    let levels: &[u32] = if weeks {
        &[CAL_YEAR, CAL_MONTH, CAL_WEEK, CAL_DAY, CAL_HOUR]
    } else {
        &[CAL_YEAR, CAL_MONTH, CAL_DAY, CAL_HOUR]
    };
    for &level in levels {
        let mine: Vec<(i128, i128)> = slots
            .iter()
            .filter(|s| s.0 == level)
            .map(|s| (s.1, s.2))
            .collect();
        let allowance = share(level).saturating_add(carry).min(budget - out.used);
        if mine.is_empty() {
            carry = allowance;
            continue;
        }
        let cutoff = if frozen {
            strike(mine.last().expect("non-empty").1)
        } else {
            i128::MAX
        };
        let start = out.cover.len();
        let mut spent = 0usize;
        let mut cal_slots = Vec::new();
        for (t, &(sa, sb)) in mine.iter().enumerate() {
            let left = allowance - spent;
            let space = left / (mine.len() - t);
            let mut slot = CalSlot {
                start: sa,
                end: sb,
                allowance: space,
                entries: 0,
                used: 0,
            };
            if space > 0 {
                let candidate: Vec<bool> = spans
                    .iter()
                    .zip(observed)
                    .map(|(s, &o)| s.0 >= sa && s.0 < sb && o <= cutoff)
                    .collect();
                let gradient = DensityGradient::new(sb - sa, space);
                let (cover, used) = walk(
                    spans,
                    costs,
                    &candidate,
                    &mut selected,
                    sa,
                    &gradient,
                    space,
                );
                slot.entries = cover.len();
                slot.used = used;
                out.cover.extend(cover);
                spent += used;
            }
            cal_slots.push(slot);
        }
        out.levels.push(CalLevel {
            level,
            slots: cal_slots,
            allowance,
            start,
            len: out.cover.len() - start,
            used: spent,
        });
        out.used += spent;
        carry = allowance - spent;
    }
    let rest = budget - out.used;
    out.open_space = rest;
    let gradient = DensityGradient::new((latest - open_start).max(1), rest);
    let candidate = vec![true; spans.len()];
    let (open, used) = walk(
        spans,
        costs,
        &candidate,
        &mut selected,
        open_start,
        &gradient,
        rest,
    );
    out.cover.extend(open);
    out.used += used;
    out
}

/// Replay the walk in use beside calendar-grid configurations
/// `(name, anchor, weeks, frozen)`.
pub fn replay_calendar<B: BlobStoreGet, P: TriblePattern>(
    space: &P,
    ws: &B,
    budget: usize,
    steps: usize,
    step_units: i128,
    configs: &[(String, i128, bool, bool)],
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
    struct Step {
        point: i128,
        base: Vec<usize>,
        base_used: usize,
        cuts: Vec<CalendarCut>,
        silent: Vec<bool>,
    }
    let mut base_silent = 0usize;
    let mut out_steps: Vec<Step> = Vec::new();
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
        let base = select_recollection_cut(&sub, &sub_costs, &vec![true; sub.len()], budget);
        if !silent_life_quarters(&sub, &base.cover).is_empty() {
            base_silent += 1;
        }
        let mut cuts = Vec::new();
        let mut silent = Vec::new();
        for (_, anchor, weeks, frozen) in configs {
            let mut cut = select_calendar_recollection_cut(
                &sub, &sub_costs, &sub_obs, budget, *anchor, *weeks, *frozen,
            );
            silent.push(!silent_life_quarters(&sub, &cut.cover).is_empty());
            for x in cut.cover.iter_mut() {
                *x = map[*x];
            }
            cuts.push(cut);
        }
        out_steps.push(Step {
            point,
            base: base.cover.iter().map(|&j| map[j]).collect(),
            base_used: base.used,
            cuts,
            silent,
        });
    }
    lines.push(format!(
        "frame: budget {budget} CHARACTERS; {} replay points ({} transitions) of {step_units} x 2^14 s, observation (created_at) {} to {}; \
         {} exact-span classes ({} with range-end fallback); figures are rendered cover characters compared as entry sequences; \
         calendar in UTC (hifitime, leap seconds included), keys TAI",
        out_steps.len(),
        out_steps.len().saturating_sub(1),
        fmt_key(out_steps.first().map_or(latest_observation, |s| s.point)),
        fmt_key(latest_observation),
        spans.len(),
        unobserved,
    ));
    let changed: Vec<usize> = (1..out_steps.len())
        .filter(|&k| out_steps[k].base != out_steps[k - 1].base)
        .collect();
    let mean = |v: &[usize]| v.iter().sum::<usize>() as f64 / v.len().max(1) as f64;
    let base_used: Vec<usize> = changed.iter().map(|&k| out_steps[k].base_used).collect();
    let base_lcp: Vec<usize> = changed
        .iter()
        .map(|&k| lcp_chars(&out_steps[k].base, &out_steps[k - 1].base, &costs))
        .collect();
    let mut summary = vec![format!(
        "walk in use: {} of {} transitions changed the cover; on those mean cover {:.0}k, reused with today's single saved prefix 0, \
         common entry prefix {:.0}k; mean fill {:.1}%; {} step(s) silent > LIFE/4",
        changed.len(),
        out_steps.len().saturating_sub(1),
        mean(&base_used) / 1e3,
        mean(&base_lcp) / 1e3,
        out_steps.iter().map(|s| pct(s.base_used)).sum::<f64>() / out_steps.len().max(1) as f64,
        base_silent,
    )];
    for (c, (name, _, weeks, _)) in configs.iter().enumerate() {
        let order: Vec<u32> = if *weeks {
            vec![CAL_YEAR, CAL_MONTH, CAL_WEEK, CAL_DAY, CAL_HOUR]
        } else {
            vec![CAL_YEAR, CAL_MONTH, CAL_DAY, CAL_HOUR]
        };
        lines.push(String::new());
        lines.push(format!(
            "== calendar {name}: levels coarsest first as name:slots:used (* = changed); resume = reused if the backend could resume at the \
             deepest unchanged level; at-<level> = reused with ONE prefix saved at the end of that level"
        ));
        lines.push(format!(
            "{:<20} {:>8} {:>6} {:>7} {:>8} {:>8} {:>8} {:>8}  levels | open",
            "observed", "used", "fill%", "open", "resume", "at-month", "at-week", "lcp"
        ));
        let mut fills = Vec::new();
        let mut silent_steps = 0usize;
        let (mut resume_v, mut lcp_v, mut used_v, mut open_v) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        let mut at_v: Vec<Vec<usize>> = vec![Vec::new(); 5];
        let mut lvl_used: Vec<Vec<usize>> = vec![Vec::new(); 5];
        let (mut own, mut cascade, mut other) = (vec![0usize; 5], vec![0usize; 5], vec![0usize; 5]);
        for (k, s) in out_steps.iter().enumerate() {
            let cut = &s.cuts[c];
            fills.push(pct(cut.used));
            if s.silent[c] {
                silent_steps += 1;
            }
            let open_used = cut.used - cut.levels.iter().map(|l| l.used).sum::<usize>();
            for l in &cut.levels {
                lvl_used[l.level as usize].push(l.used);
            }
            let mut cells = Vec::new();
            let (mut resume, mut lcp) = (0usize, 0usize);
            let mut at = vec![0usize; 5];
            if k > 0 {
                let prev = &out_steps[k - 1].cuts[c];
                let mut intact = prev.years == cut.years;
                let mut coarser_changed = !intact;
                let mut through: Vec<(u32, bool, usize)> = Vec::new();
                for &l in &order {
                    let same_span = cut.level_span(l) == prev.level_span(l);
                    let same = prev.years == cut.years
                        && same_span
                        && cut.level_entries(l) == prev.level_entries(l);
                    let used = cut
                        .levels
                        .iter()
                        .find(|x| x.level == l)
                        .map_or(0, |x| x.used);
                    if same && intact {
                        resume += used;
                    } else {
                        intact = false;
                    }
                    if !same {
                        if coarser_changed {
                            cascade[l as usize] += 1;
                        } else if !same_span {
                            own[l as usize] += 1;
                        } else {
                            other[l as usize] += 1;
                        }
                        coarser_changed = true;
                    }
                    through.push((l, same, used));
                    if let Some(x) = cut.levels.iter().find(|x| x.level == l) {
                        cells.push(format!(
                            "{}{}:{}:{}",
                            if same { "" } else { "*" },
                            cal_level_name(l),
                            x.slots.len(),
                            x.used
                        ));
                    }
                }
                for (j, &(l, _, _)) in through.iter().enumerate() {
                    if through[..=j].iter().all(|&(_, s, _)| s) {
                        at[l as usize] = through[..=j].iter().map(|&(_, _, u)| u).sum();
                    }
                }
                lcp = lcp_chars(&cut.cover, &prev.cover, &costs);
                if changed.contains(&k) {
                    resume_v.push(resume);
                    for l in 0..5 {
                        at_v[l].push(at[l]);
                    }
                    lcp_v.push(lcp);
                    used_v.push(cut.used);
                    open_v.push(open_used);
                }
            } else {
                for x in &cut.levels {
                    cells.push(format!(
                        "{}:{}:{}",
                        cal_level_name(x.level),
                        x.slots.len(),
                        x.used
                    ));
                }
            }
            lines.push(format!(
                "{:<20} {:>8} {:>5.1}% {:>7} {:>8} {:>8} {:>8} {:>8}  {} | {}{}",
                fmt_key(s.point),
                cut.used,
                pct(cut.used),
                open_used,
                resume,
                at[CAL_MONTH as usize],
                if *weeks { at[CAL_WEEK as usize] } else { 0 },
                lcp,
                cells.join(" "),
                open_used,
                if s.silent[c] { "  SILENT>LIFE/4" } else { "" }
            ));
        }
        let total = |v: &[usize]| v.iter().sum::<usize>();
        let reread = |v: &[usize]| (total(&used_v) - total(v)) as f64 / 1e6;
        let ats: Vec<String> = order
            .iter()
            .map(|&l| {
                let v = &at_v[l as usize];
                let zero = v.iter().filter(|&&x| x == 0).count();
                format!(
                    "at-{} {:.0}k (reread {:.1}M, zero reuse on {zero})",
                    cal_level_name(l),
                    mean(v) / 1e3,
                    reread(v)
                )
            })
            .collect();
        let lv: Vec<String> = order
            .iter()
            .map(|&l| {
                let i = l as usize;
                format!(
                    "{}: mean {:.0}k chars over {} steps present; changed on {} transitions: {} by its own slot set, {} by a coarser change, {} by content alone",
                    cal_level_name(l),
                    mean(&lvl_used[i]) / 1e3,
                    lvl_used[i].len(),
                    own[i] + cascade[i] + other[i],
                    own[i],
                    cascade[i],
                    other[i]
                )
            })
            .collect();
        summary.push(format!(
            "calendar {name}: mean fill {:.1}% (min {:.1}%), {silent_steps} step(s) silent > LIFE/4; on the {} changed transitions (mean cover {:.0}k, \
             open hour mean {:.0}k): reused mean resume-at-deepest-unchanged {:.0}k (reread {:.1}M), {}, common entry prefix {:.0}k (reread {:.1}M); \
             walk in use: single prefix reread {:.1}M, common-prefix reread {:.1}M\n    {}",
            fills.iter().sum::<f64>() / fills.len().max(1) as f64,
            fills.iter().copied().fold(f64::INFINITY, f64::min),
            used_v.len(),
            mean(&used_v) / 1e3,
            mean(&open_v) / 1e3,
            mean(&resume_v) / 1e3,
            reread(&resume_v),
            ats.join(", "),
            mean(&lcp_v) / 1e3,
            reread(&lcp_v),
            total(&base_used) as f64 / 1e6,
            (total(&base_used) - total(&base_lcp)) as f64 / 1e6,
            lv.join("\n    "),
        ));
    }
    // The slot structure at the last step, for the policy-(b) configurations.
    if let Some(last) = out_steps.last() {
        for (c, (name, _, _, frozen)) in configs.iter().enumerate() {
            if *frozen || !name.contains("moment") {
                continue;
            }
            let cut = &last.cuts[c];
            lines.push(String::new());
            lines.push(format!(
                "== slot structure at {} for calendar {name}",
                fmt_key(last.point)
            ));
            for l in &cut.levels {
                for s in &l.slots {
                    lines.push(format!(
                        "  {:<5} {}  allowance {:>7}  used {:>7}  entries {:>4}",
                        cal_level_name(l.level),
                        format_time_range(key_to_epoch(s.start), key_to_epoch(s.end)),
                        s.allowance,
                        s.used,
                        s.entries
                    ));
                }
            }
            let open_used = cut.used - cut.levels.iter().map(|l| l.used).sum::<usize>();
            lines.push(format!(
                "  open  from {}  space {:>7}  used {:>7}  (today's walk; may fall back on any memory not yet taken)",
                fmt_key(cut.open_start),
                cut.open_space,
                open_used
            ));
        }
    }
    lines.push(String::new());
    lines.push("== summary".into());
    lines.extend(summary);
    Ok(lines)
}

// ---------------------------------------------------------------------------
// tiled recollection: the old LSM's base-4 tile counter, the greedy walk in each tile
// ---------------------------------------------------------------------------

/// One level of a [`TiledCut`]: its complete tiles, rendered in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TileLevel {
    /// Tiles at this level are `4^level` units of 2^14 s wide.
    pub level: u32,
    /// Complete tiles at this level (the base-4 digit, 1..=3).
    pub tiles: usize,
    /// The level's span `[lower, upper)` (absolute TAI keys).
    pub lower: i128,
    pub upper: i128,
    /// Characters the level was allowed (its share plus what older levels
    /// left unspent).
    pub allowance: usize,
    /// Entries `cover[start..start + len]` belong to this level.
    pub start: usize,
    pub len: usize,
    pub used: usize,
}

/// A recollection rendered as base-4 tile levels, coarsest first, then the
/// open unit and its fallback walk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TiledCut {
    pub cover: Vec<usize>,
    pub levels: Vec<TileLevel>,
    /// The highest level the counter uses for this life.
    pub top: u32,
    /// Start of the open unit (absolute key) and the first open-walk entry.
    pub open_start: i128,
    pub open_from: usize,
    /// Characters left to the open walk (residual share plus unspent).
    pub open_space: usize,
    /// The open walk's own share; `open_space - open_share` is what the
    /// closed levels left unspent.
    pub open_share: usize,
    /// Append-only open slot or open run: memories that did not fit.
    pub open_overflow: usize,
    /// Runs: memories that start in a closed run and end past its seam.
    pub crossing: usize,
    pub used: usize,
}

impl TiledCut {
    pub fn level_entries(&self, level: u32) -> &[usize] {
        self.levels
            .iter()
            .find(|l| l.level == level)
            .map_or(&[][..], |l| &self.cover[l.start..l.start + l.len])
    }
}

/// The declared constants of one tiled configuration.
#[derive(Clone, Debug, PartialEq)]
pub struct TiledOpts {
    /// Anchor of the per-level shares: 3 s (today's shape) or 2^14 s (the
    /// 2026-09-05 tiled shape).
    pub anchor_ns: i128,
    /// Arrival policy a (true): a level's candidates are those observed by its
    /// latest change. Policy b (false): every memory is a candidate at once.
    pub frozen: bool,
    /// Inside a tile, target slots uniformly over the tile (the old design's
    /// one grain per tile) instead of today's log gradient toward the tile's
    /// young edge.
    pub uniform: bool,
    /// Policy a only: levels whose tiles are wider than this re-read their
    /// candidates at least once per this much OBSERVATION time (the newest
    /// created_at in the pile, floored to a multiple of the cap), bounding
    /// how long a late summary waits.
    pub cap_ns: Option<i128>,
    /// A declared closed fraction instead of anchor shares: the closed levels
    /// together get this fraction of the budget, equally per level (JP,
    /// 2026-11-08: "2/4 big picture history, 1/4 current context, 1/4 now",
    /// with "now" the resident's live quarter after the cover, so 2/3 of the
    /// cover closed and 1/3 open). `anchor_ns` is then unused.
    pub closed_fraction: Option<f64>,
    /// Clock-closed APPEND-ONLY open slot: when set, the open unit holds
    /// exactly the memories that start in it, in range-time order, with no
    /// fallback filler, and this fraction of the budget is reserved for it;
    /// the closed levels share the rest (see `deep_weights`).
    pub open_append: Option<f64>,
    /// With `open_append`: closed shares weighted `min(1, 4^(l-2))` (levels 0
    /// and 1 light, where support is thin) instead of equal per level.
    pub deep_weights: bool,
    /// FULL-CLOSED runs instead of clock tiles: the open run closes when the
    /// memories appended to it reach this fraction of the budget, and closed
    /// runs carry base 4 by rank. See [`select_runs_recollection_cut`].
    pub runs: Option<f64>,
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

/// A target map inside one slot.
trait Target {
    fn at(&self, cursor: usize) -> f64;
}

impl Target for DensityGradient {
    fn at(&self, cursor: usize) -> f64 {
        self.sample(cursor).time_ns
    }
}

/// Uniform targets over `[0, life]` for `space` characters.
struct Uniform {
    life_ns: f64,
    space: usize,
}

impl Target for Uniform {
    fn at(&self, cursor: usize) -> f64 {
        self.life_ns * cursor.min(self.space) as f64 / self.space.max(1) as f64
    }
}

/// [`walk`] over any target map.
fn walk_targets(
    spans: &[(i128, i128, Id)],
    costs: &[usize],
    candidate: &[bool],
    selected: &mut [bool],
    origin: i128,
    target: &dyn Target,
    cap: usize,
) -> (Vec<usize>, usize) {
    let mut cover = Vec::new();
    let mut used = 0usize;
    while used < cap {
        let ideal_start = target.at(used);
        let mut best: Option<(f64, Id, usize)> = None;
        for (i, &(start, end, id)) in spans.iter().enumerate() {
            let cost = costs[i];
            if selected[i] || !candidate[i] || cost == 0 {
                continue;
            }
            let width = end.saturating_sub(start).max(MOMENT_NS) as f64;
            let actual_start = start.saturating_sub(origin) as f64;
            let actual_end = actual_start + width;
            let ideal_end = target.at(used.saturating_add(cost));
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

/// The tiled selector.
///
/// The tiling is the old tiled cover's (wiki "The memory cover", 2026-09-05):
/// an absolute grid of 2^14 s units, tiles quadrupling backwards, and at most
/// three complete tiles per level before the current block of the next level.
/// It is the base-4 counter of `n`, the unit holding the newest memory end:
/// level `l` spans `[floor(n/4^(l+1))*4^(l+1), floor(n/4^l)*4^l)` units, so its
/// tile count is the base-4 digit `l` of `n`.
///
/// Each level gets a fixed share of the budget from the log-age gradient over
/// the counter's own bands, `ln((A + 4^(l+1)u)/(A + 4^l u)) / ln((A +
/// 4^(top+1)u)/A)`, never a globally fitted detail. A level's allowance
/// (share plus what older levels left unspent) is split evenly among its
/// tiles, oldest first, unspent passing on. Inside each tile today's greedy
/// walk chooses among memories that START in the tile, against either today's
/// log targets toward the tile's young edge or uniform targets
/// ([`TiledOpts::uniform`]). Arrival: policy a freezes a level's candidates at
/// its latest change (optionally refreshed every [`TiledOpts::cap_ns`] of
/// observation time for wide levels); policy b admits every memory at once.
/// What all levels leave unspent goes to today's walk over the open unit
/// `[n*u, L]`, which may fall back on any memory not yet taken.
///
/// Pure in the spans, their first observation times, the budget and the
/// declared options. No previous cover, no wall clock, no completeness floor.
/// `late` = `(level, shift)` moves one level's cutoff earlier, for the
/// late-arrival probe; the real selector passes `None`.
pub fn select_tiled_recollection_cut(
    spans: &[(i128, i128, Id)],
    costs: &[usize],
    observed: &[i128],
    budget: usize,
    opts: &TiledOpts,
    late: Option<(u32, i128)>,
) -> TiledCut {
    assert_eq!(spans.len(), costs.len());
    assert_eq!(spans.len(), observed.len());
    let mut out = TiledCut {
        cover: Vec::new(),
        levels: Vec::new(),
        top: 0,
        open_start: 0,
        open_from: 0,
        open_space: 0,
        open_share: 0,
        open_overflow: 0,
        crossing: 0,
        used: 0,
    };
    if let Some(open_frac) = opts.runs {
        return select_runs_recollection_cut(spans, costs, observed, budget, open_frac, opts);
    }
    let (Some(earliest), Some(latest)) = (
        spans.iter().map(|s| s.0).min(),
        spans.iter().map(|s| s.1).max(),
    ) else {
        return out;
    };
    if budget == 0 {
        return out;
    }
    let unit = SEAL_UNIT_NS;
    let n = latest.div_euclid(unit);
    let first = earliest.div_euclid(unit);
    let block = |l: u32| -> i128 {
        let w = 4i128.pow(l);
        n.div_euclid(w) * w
    };
    let mut top = 0u32;
    while block(top + 1) > first {
        top += 1;
    }
    out.top = top;
    let a = opts.anchor_ns.max(1) as f64;
    let u = unit as f64;
    let norm = ((a + 4f64.powi(top as i32 + 1) * u) / a).ln();
    let weight = |l: u32| -> f64 {
        if opts.deep_weights {
            4f64.powi(l as i32 - 2).min(1.0)
        } else {
            1.0
        }
    };
    let weights: f64 = (0..=top).map(weight).sum();
    let share = |l: u32| -> usize {
        if let Some(r) = opts.open_append {
            return ((1.0 - r.clamp(0.0, 1.0)) * budget as f64 * weight(l) / weights).floor()
                as usize;
        }
        if let Some(f) = opts.closed_fraction {
            return (f.clamp(0.0, 1.0) * budget as f64 / (top as f64 + 1.0)).floor() as usize;
        }
        let hi = a + 4f64.powi(l as i32 + 1) * u;
        let lo = a + 4f64.powi(l as i32) * u;
        ((hi / lo).ln() / norm * budget as f64).floor() as usize
    };
    out.open_share = budget - (0..=top).map(share).sum::<usize>().min(budget);

    let mut order: Vec<usize> = (0..spans.len()).collect();
    order.sort_by(|&x, &y| {
        observed[x]
            .cmp(&observed[y])
            .then(spans[x].2.cmp(&spans[y].2))
    });
    let mut edges: Vec<(i128, i128)> = Vec::with_capacity(order.len());
    let mut edge = i128::MIN;
    for &i in &order {
        edge = edge.max(spans[i].1);
        edges.push((edge, observed[i]));
    }
    let strike = |end: i128| -> i128 {
        let k = edges.partition_point(|&(e, _)| e < end);
        edges.get(k).map_or(i128::MAX, |&(_, o)| o)
    };
    let newest_observation = observed.iter().copied().max().unwrap_or(0);

    let mut selected = vec![false; spans.len()];
    let mut carry = 0usize;
    for l in (0..=top).rev() {
        let width = 4i128.pow(l);
        let (lo, hi) = (block(l + 1), block(l));
        let tiles = ((hi - lo) / width) as usize;
        let allowance = share(l).saturating_add(carry).min(budget - out.used);
        if tiles == 0 {
            carry = allowance;
            continue;
        }
        let shift = match late {
            Some((level, shift)) if level == l => shift,
            _ => 0,
        };
        // Policy a: the level's latest change (the strike of its newest tile
        // end), so a late summary enters at the level's next change; with a
        // cap, wide levels also re-read at least once per cap of observation
        // time. Policy b: no cutoff.
        let cutoff = if opts.frozen {
            let mut c = strike(hi * unit);
            if let Some(cap) = opts.cap_ns.filter(|&cap| width * unit > cap) {
                c = c.max(newest_observation.div_euclid(cap) * cap);
            }
            c.saturating_sub(shift)
        } else {
            i128::MAX
        };
        let start = out.cover.len();
        let mut spent = 0usize;
        for t in 0..tiles {
            let ta = (lo + t as i128 * width) * unit;
            let tb = ta + width * unit;
            let left = allowance - spent;
            let tile_space = left / (tiles - t);
            if tile_space == 0 {
                continue;
            }
            let candidate: Vec<bool> = spans
                .iter()
                .zip(observed)
                .map(|(s, &o)| s.0 >= ta && s.0 < tb && o <= cutoff)
                .collect();
            let (cover, used) = if opts.uniform {
                let target = Uniform {
                    life_ns: (tb - ta) as f64,
                    space: tile_space,
                };
                walk_targets(
                    spans,
                    costs,
                    &candidate,
                    &mut selected,
                    ta,
                    &target,
                    tile_space,
                )
            } else {
                let gradient = DensityGradient::new(tb - ta, tile_space);
                walk(
                    spans,
                    costs,
                    &candidate,
                    &mut selected,
                    ta,
                    &gradient,
                    tile_space,
                )
            };
            out.cover.extend(cover);
            spent += used;
        }
        out.levels.push(TileLevel {
            level: l,
            tiles,
            lower: lo * unit,
            upper: hi * unit,
            allowance,
            start,
            len: out.cover.len() - start,
            used: spent,
        });
        out.used += spent;
        carry = allowance - spent;
    }
    let rest = budget - out.used;
    out.open_space = rest;
    let origin = n * unit;
    out.open_start = origin;
    out.open_from = out.cover.len();
    if opts.open_append.is_some() {
        let mut members: Vec<usize> = (0..spans.len())
            .filter(|&i| !selected[i] && spans[i].0 >= origin && costs[i] > 0)
            .collect();
        range_time_sort(&mut members, spans);
        let mut used = 0usize;
        for (k, &i) in members.iter().enumerate() {
            if used + costs[i] > rest {
                out.open_overflow = members.len() - k;
                break;
            }
            used += costs[i];
            out.cover.push(i);
        }
        out.used += used;
        return out;
    }
    let gradient = DensityGradient::new(latest - origin, rest);
    let candidate = vec![true; spans.len()];
    let (open, used) = walk(
        spans,
        costs,
        &candidate,
        &mut selected,
        origin,
        &gradient,
        rest,
    );
    out.cover.extend(open);
    out.used += used;
    out
}

/// FULL-CLOSED runs ("causal radix runs", wiki "Causal radix cover: the carry
/// is necessary; the clock tile is not"), with JP's 2026-11-08 trigger: the
/// open slot is "appended until it's full and then turned into a compacted
/// slot".
///
/// - Memories are taken in observation order (first `created_at`, then
///   intrinsic id). Each one that STARTS at or after the current seam is
///   appended to the open run and adds its rendered size to the run's mass.
/// - When that mass reaches `open_frac` of the budget, the run closes. Its
///   seam is the greatest end among its members (the lived edge it reached),
///   so no member crosses its own seam and no seam ever has to move left. A
///   memory observed later that starts before a seam belongs to the run
///   holding its start (the oldest run it touches); `crossing` counts those
///   that end past the seam.
/// - Closed runs carry base 4 by rank: a closing run is rank 0; four adjacent
///   runs of one rank merge into one of the next. So at most three runs of a
///   rank remain, and a rank-r run changes once per `4^r` closes.
/// - The closed runs share `1 - open_frac` of the budget, equally per rank and
///   evenly over a rank's runs, unspent passing to younger runs. A run renders
///   everything that starts in it when that fits, otherwise today's walk with
///   uniform targets over the run picks a subset; either way the run is
///   emitted in range-time order. Arrival is policy b: every memory is a
///   candidate at once.
/// - The open run renders all its members in range-time order, as the
///   append-only memtable.
///
/// Seams are a function of the observation-ordered prefix only, so later
/// observations never move an earlier seam (late replication that inserts an
/// OLD created_at into the order can).
pub fn select_runs_recollection_cut(
    spans: &[(i128, i128, Id)],
    costs: &[usize],
    observed: &[i128],
    budget: usize,
    open_frac: f64,
    opts: &TiledOpts,
) -> TiledCut {
    let mut out = TiledCut {
        cover: Vec::new(),
        levels: Vec::new(),
        top: 0,
        open_start: 0,
        open_from: 0,
        open_space: 0,
        open_share: 0,
        open_overflow: 0,
        crossing: 0,
        used: 0,
    };
    let Some(earliest) = spans.iter().map(|s| s.0).min() else {
        return out;
    };
    if budget == 0 {
        return out;
    }
    let quantum = ((open_frac.clamp(0.0, 1.0) * budget as f64).floor() as usize).max(1);
    out.open_share = quantum;
    let mut order: Vec<usize> = (0..spans.len()).collect();
    order.sort_by(|&x, &y| {
        observed[x]
            .cmp(&observed[y])
            .then(spans[x].2.cmp(&spans[y].2))
    });
    // (rank, lo, hi) runs, oldest first; ranks never increase along the list.
    let mut runs: Vec<(u32, i128, i128)> = Vec::new();
    let mut seam = earliest;
    let mut mass = 0usize;
    let mut edge = i128::MIN;
    for &i in &order {
        if spans[i].0 < seam {
            continue;
        }
        mass += costs[i];
        edge = edge.max(spans[i].1);
        if mass >= quantum {
            let hi = edge.max(seam + 1);
            runs.push((0, seam, hi));
            seam = hi;
            mass = 0;
            edge = i128::MIN;
            while runs.len() >= 4 {
                let n = runs.len();
                let r = runs[n - 1].0;
                if runs[n - 4..].iter().all(|x| x.0 == r) {
                    let lo = runs[n - 4].1;
                    let hi = runs[n - 1].2;
                    runs.truncate(n - 4);
                    runs.push((r + 1, lo, hi));
                } else {
                    break;
                }
            }
        }
    }
    let top = runs.iter().map(|r| r.0).max().unwrap_or(0);
    out.top = top;
    let closed = budget - quantum.min(budget);
    let per_rank = closed / (top as usize + 1);
    let mut selected = vec![false; spans.len()];
    let mut carry = 0usize;
    for rank in (0..=top).rev() {
        let mine: Vec<(i128, i128)> = runs
            .iter()
            .filter(|r| r.0 == rank)
            .map(|r| (r.1, r.2))
            .collect();
        let allowance = per_rank.saturating_add(carry).min(budget - out.used);
        if mine.is_empty() {
            carry = allowance;
            continue;
        }
        let start = out.cover.len();
        let mut spent = 0usize;
        for (t, &(lo, hi)) in mine.iter().enumerate() {
            let space = (allowance - spent) / (mine.len() - t);
            let members: Vec<usize> = (0..spans.len())
                .filter(|&i| !selected[i] && spans[i].0 >= lo && spans[i].0 < hi && costs[i] > 0)
                .collect();
            out.crossing += members.iter().filter(|&&i| spans[i].1 > hi).count();
            let mass: usize = members.iter().map(|&i| costs[i]).sum();
            let mut chosen = if mass <= space {
                members
            } else {
                let mut candidate = vec![false; spans.len()];
                for &i in &members {
                    candidate[i] = true;
                }
                let target = Uniform {
                    life_ns: (hi - lo) as f64,
                    space,
                };
                walk_targets(
                    spans,
                    costs,
                    &candidate,
                    &mut vec![false; spans.len()],
                    lo,
                    &target,
                    space,
                )
                .0
            };
            range_time_sort(&mut chosen, spans);
            for &i in &chosen {
                selected[i] = true;
                spent += costs[i];
            }
            out.cover.extend(chosen);
        }
        out.levels.push(TileLevel {
            level: rank,
            tiles: mine.len(),
            lower: mine[0].0,
            upper: mine[mine.len() - 1].1,
            allowance,
            start,
            len: out.cover.len() - start,
            used: spent,
        });
        out.used += spent;
        carry = allowance - spent;
    }
    let rest = budget - out.used;
    out.open_space = rest;
    out.open_start = seam;
    out.open_from = out.cover.len();
    let mut members: Vec<usize> = (0..spans.len())
        .filter(|&i| !selected[i] && spans[i].0 >= seam && costs[i] > 0)
        .collect();
    range_time_sort(&mut members, spans);
    let mut used = 0usize;
    for (k, &i) in members.iter().enumerate() {
        if used + costs[i] > rest {
            out.open_overflow = members.len() - k;
            break;
        }
        used += costs[i];
        out.cover.push(i);
    }
    out.used += used;
    let _ = opts;
    out
}

/// Characters of `entries` by where each memory STARTS relative to `latest`:
/// [inside the open unit, earlier within 1 day, 1 to 7 days, 7 to 30 days, older].
fn age_split(
    entries: &[usize],
    spans: &[(i128, i128, Id)],
    costs: &[usize],
    latest: i128,
    open_start: i128,
) -> [usize; 5] {
    const DAY: i128 = 86_400 * 1_000_000_000;
    let mut out = [0usize; 5];
    for &i in entries {
        let s = spans[i].0;
        let b = if s >= open_start {
            0
        } else if s >= latest - DAY {
            1
        } else if s >= latest - 7 * DAY {
            2
        } else if s >= latest - 30 * DAY {
            3
        } else {
            4
        };
        out[b] += costs[i];
    }
    out
}

/// Replay the walk in use beside tiled configurations.
pub fn replay_tiled<B: BlobStoreGet, P: TriblePattern>(
    space: &P,
    ws: &B,
    budget: usize,
    steps: usize,
    step_units: i128,
    configs: &[(String, TiledOpts)],
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
    const LAGS_H: [i128; 3] = [1, 6, 24];
    const MAX_LEVEL: usize = 16;

    struct Step {
        point: i128,
        latest: i128,
        base: Vec<usize>,
        base_used: usize,
        cuts: Vec<TiledCut>,
        silent: Vec<bool>,
    }
    let mut probes = vec![vec![(0usize, [0usize; 3]); MAX_LEVEL]; configs.len()];
    let mut seen: Vec<Vec<Vec<(i128, i128, usize, u32, i128)>>> =
        vec![vec![Vec::new(); MAX_LEVEL]; configs.len()];
    let mut base_silent = 0usize;
    let mut out_steps: Vec<Step> = Vec::new();
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
        let latest = sub.iter().map(|s| s.1).max().unwrap_or(0);
        let newest_obs = sub_obs.iter().copied().max().unwrap_or(0);
        let base = select_recollection_cut(&sub, &sub_costs, &vec![true; sub.len()], budget);
        if !silent_life_quarters(&sub, &base.cover).is_empty() {
            base_silent += 1;
        }
        let mut cuts = Vec::new();
        let mut silent = Vec::new();
        for (c, (_, opts)) in configs.iter().enumerate() {
            let mut cut =
                select_tiled_recollection_cut(&sub, &sub_costs, &sub_obs, budget, opts, None);
            silent.push(!silent_life_quarters(&sub, &cut.cover).is_empty());
            if opts.frozen {
                for lv in &cut.levels {
                    let li = lv.level as usize;
                    if li >= MAX_LEVEL {
                        continue;
                    }
                    let capped = opts
                        .cap_ns
                        .filter(|&cap| 4i128.pow(lv.level) * SEAL_UNIT_NS > cap)
                        .map_or(0, |cap| newest_obs.div_euclid(cap));
                    let key = (lv.lower, lv.upper, lv.allowance, cut.top, capped);
                    if seen[c][li].contains(&key) {
                        continue;
                    }
                    seen[c][li].push(key);
                    probes[c][li].0 += 1;
                    for (j, lag_h) in LAGS_H.iter().enumerate() {
                        let lag = lag_h * 3600 * 1_000_000_000;
                        let late = select_tiled_recollection_cut(
                            &sub,
                            &sub_costs,
                            &sub_obs,
                            budget,
                            opts,
                            Some((lv.level, lag)),
                        );
                        if late.level_entries(lv.level) != cut.level_entries(lv.level) {
                            probes[c][li].1[j] += 1;
                        }
                    }
                }
            }
            for x in cut.cover.iter_mut() {
                *x = map[*x];
            }
            cuts.push(cut);
        }
        out_steps.push(Step {
            point,
            latest,
            base: base.cover.iter().map(|&j| map[j]).collect(),
            base_used: base.used,
            cuts,
            silent,
        });
    }

    lines.push(format!(
        "frame: budget {budget} CHARACTERS; {} replay points ({} transitions) of {step_units} x 2^14 s, observation (created_at) {} to {}; \
         {} exact-span classes ({} with range-end fallback for missing created_at); all figures are rendered cover characters, \
         compared as entry sequences; no tokens, no beliefs/goals/live turns",
        out_steps.len(),
        out_steps.len().saturating_sub(1),
        fmt_key(out_steps.first().map_or(latest_observation, |s| s.point)),
        fmt_key(latest_observation),
        spans.len(),
        unobserved,
    ));
    let changed: Vec<usize> = (1..out_steps.len())
        .filter(|&k| out_steps[k].base != out_steps[k - 1].base)
        .collect();
    let mean = |v: &[usize]| v.iter().sum::<usize>() as f64 / v.len().max(1) as f64;
    let mean_split = |v: &[[usize; 5]]| -> String {
        let n = v.len().max(1) as f64;
        let m: Vec<String> = (0..5)
            .map(|b| {
                format!(
                    "{:.0}k",
                    v.iter().map(|x| x[b]).sum::<usize>() as f64 / n / 1e3
                )
            })
            .collect();
        format!(
            "open unit {}, <1 d {}, 1-7 d {}, 7-30 d {}, older {}",
            m[0], m[1], m[2], m[3], m[4]
        )
    };
    let base_used: Vec<usize> = changed.iter().map(|&k| out_steps[k].base_used).collect();
    let base_lcp: Vec<usize> = changed
        .iter()
        .map(|&k| lcp_chars(&out_steps[k].base, &out_steps[k - 1].base, &costs))
        .collect();
    let base_split: Vec<[usize; 5]> = out_steps
        .iter()
        .map(|s| {
            let open = s.latest.div_euclid(SEAL_UNIT_NS) * SEAL_UNIT_NS;
            age_split(&s.base, &spans, &costs, s.latest, open)
        })
        .collect();
    let mut summary = vec![format!(
        "walk in use: {} of {} transitions changed the cover; on those, mean cover {:.0}k chars, reused with today's single saved prefix 0, \
         mean common entry prefix {:.0}k chars; mean fill {:.1}%; {} step(s) silent > LIFE/4\n    \
         whole cover by memory start, mean over all steps: {}",
        changed.len(),
        out_steps.len().saturating_sub(1),
        mean(&base_used) / 1e3,
        mean(&base_lcp) / 1e3,
        out_steps.iter().map(|s| pct(s.base_used)).sum::<f64>() / out_steps.len().max(1) as f64,
        base_silent,
        mean_split(&base_split),
    )];

    for (c, (name, _)) in configs.iter().enumerate() {
        lines.push(String::new());
        lines.push(format!(
            "== tiled {name}: levels coarsest first as level:tiles:used (* = changed since the previous step); \
             resume = chars reused if the backend could resume at the deepest unchanged level boundary; \
             at-j = chars reused with ONE prefix saved at the young end of level j; open split = open-walk chars by memory start \
             (open unit / <1 d / 1-7 d / 7-30 d / older)"
        ));
        lines.push(format!(
            "{:<20} {:>8} {:>6} {:>7} {:>8} {:>8} {:>8} {:>8}  {:<32}  levels",
            "observed", "used", "fill%", "open", "resume", "at-1", "at-2", "lcp", "open split"
        ));
        let mut fills = Vec::new();
        let mut silent_steps = 0usize;
        let mut resume_v = Vec::new();
        let mut at_v: Vec<Vec<usize>> = vec![Vec::new(); MAX_LEVEL];
        let mut lcp_v = Vec::new();
        let mut used_v = Vec::new();
        let mut open_v = Vec::new();
        let mut flow_v = Vec::new();
        let mut open_split_v = Vec::new();
        let mut whole_split_v = Vec::new();
        let mut insertions = 0usize;
        let mut overflow_steps = 0usize;
        let mut closed_changed = 0usize;
        let mut open_rolled = 0usize;
        let mut crossing_last = 0usize;
        let mut lvl_used: Vec<Vec<usize>> = vec![Vec::new(); MAX_LEVEL];
        let mut lvl_changed = vec![0usize; MAX_LEVEL];
        let mut lvl_own = vec![0usize; MAX_LEVEL];
        let mut lvl_content = vec![0usize; MAX_LEVEL];
        let mut max_top = 0u32;
        for (k, s) in out_steps.iter().enumerate() {
            let cut = &s.cuts[c];
            max_top = max_top.max(cut.top);
            fills.push(pct(cut.used));
            if s.silent[c] {
                silent_steps += 1;
            }
            let open_used = cut.used - cut.levels.iter().map(|l| l.used).sum::<usize>();
            flow_v.push(cut.open_space.saturating_sub(cut.open_share));
            if cut.open_overflow > 0 {
                overflow_steps += 1;
            }
            crossing_last = cut.crossing;
            if k > 0 {
                let prev = &out_steps[k - 1].cuts[c];
                let closed_same = prev.open_from == cut.open_from
                    && prev.cover[..prev.open_from] == cut.cover[..cut.open_from];
                if !closed_same {
                    closed_changed += 1;
                }
                if prev.open_start != cut.open_start {
                    open_rolled += 1;
                } else if !cut.cover[cut.open_from..].starts_with(&prev.cover[prev.open_from..]) {
                    insertions += 1;
                }
            }
            let open_split = age_split(
                &cut.cover[cut.open_from..],
                &spans,
                &costs,
                s.latest,
                cut.open_start,
            );
            open_split_v.push(open_split);
            whole_split_v.push(age_split(
                &cut.cover,
                &spans,
                &costs,
                s.latest,
                cut.open_start,
            ));
            let mut cells = Vec::new();
            let mut resume = 0usize;
            let mut at = vec![0usize; MAX_LEVEL];
            let mut lcp = 0usize;
            if k > 0 {
                let prev = &out_steps[k - 1].cuts[c];
                let same_top = prev.top == cut.top;
                let mut intact = same_top;
                let mut through: Vec<(u32, bool, usize)> = Vec::new();
                for l in (0..=cut.top).rev() {
                    let cur = cut.levels.iter().find(|x| x.level == l);
                    let old = prev.levels.iter().find(|x| x.level == l);
                    let same_span =
                        cur.map(|x| (x.lower, x.upper)) == old.map(|x| (x.lower, x.upper));
                    let same =
                        same_top && same_span && cut.level_entries(l) == prev.level_entries(l);
                    let used = cur.map_or(0, |x| x.used);
                    if same && intact {
                        resume += used;
                    } else {
                        intact = false;
                    }
                    if !same {
                        lvl_changed[l as usize] += 1;
                        if through.iter().all(|&(_, s, _)| s) && same_top {
                            if same_span {
                                lvl_content[l as usize] += 1;
                            } else {
                                lvl_own[l as usize] += 1;
                            }
                        }
                    }
                    through.push((l, same, used));
                    if let Some(x) = cur {
                        cells.push(format!(
                            "{}{}:{}:{}",
                            if same { "" } else { "*" },
                            l,
                            x.tiles,
                            x.used
                        ));
                    } else if !same {
                        cells.push(format!("*{l}:0:0"));
                    }
                }
                for j in 0..=cut.top as usize {
                    let rows: Vec<_> = through
                        .iter()
                        .filter(|&&(l, _, _)| l as usize >= j)
                        .collect();
                    if rows.iter().all(|&&(_, s, _)| s) {
                        at[j] = rows.iter().map(|&&(_, _, u)| u).sum();
                    }
                }
                lcp = lcp_chars(&cut.cover, &prev.cover, &costs);
                if changed.contains(&k) {
                    resume_v.push(resume);
                    for j in 0..MAX_LEVEL {
                        at_v[j].push(at[j]);
                    }
                    lcp_v.push(lcp);
                    used_v.push(cut.used);
                    open_v.push(open_used);
                }
            } else {
                for x in &cut.levels {
                    cells.push(format!("{}:{}:{}", x.level, x.tiles, x.used));
                }
            }
            for x in &cut.levels {
                lvl_used[x.level as usize].push(x.used);
            }
            let split = format!(
                "{}/{}/{}/{}/{}",
                open_split[0] / 1000,
                open_split[1] / 1000,
                open_split[2] / 1000,
                open_split[3] / 1000,
                open_split[4] / 1000
            );
            lines.push(format!(
                "{:<20} {:>8} {:>5.1}% {:>7} {:>8} {:>8} {:>8} {:>8}  {:<32}  {}{}",
                fmt_key(s.point),
                cut.used,
                pct(cut.used),
                open_used,
                resume,
                at[1],
                at[2],
                lcp,
                split,
                cells.join(" "),
                if s.silent[c] { "  SILENT>LIFE/4" } else { "" }
            ));
        }
        let total = |v: &[usize]| v.iter().sum::<usize>();
        let reread = |v: &[usize]| (total(&used_v) - total(v)) as f64 / 1e6;
        let ats: Vec<String> = (0..=max_top as usize)
            .map(|j| {
                let zero = at_v[j].iter().filter(|&&x| x == 0).count();
                format!(
                    "at-{j} {:.0}k (reread {:.1}M, no reuse on {zero})",
                    mean(&at_v[j]) / 1e3,
                    reread(&at_v[j])
                )
            })
            .collect();
        let lvls: Vec<String> = (0..=max_top as usize)
            .rev()
            .map(|l| {
                let p = &probes[c][l];
                let probe = if configs[c].1.frozen {
                    format!(
                        "; late-arrival probe changes {}/{} level states at 1h, {}/{} at 6h, {}/{} at 24h",
                        p.1[0], p.0, p.1[1], p.0, p.1[2], p.0
                    )
                } else {
                    String::new()
                };
                format!(
                    "level {l} (tiles {}): mean {:.0}k chars over {} steps present; changed on {} transitions \
                     ({} by its own digit, {} by content alone, rest by a coarser change){probe}",
                    humanize_unit(l as u32),
                    mean(&lvl_used[l]) / 1e3,
                    lvl_used[l].len(),
                    lvl_changed[l],
                    lvl_own[l],
                    lvl_content[l],
                )
            })
            .collect();
        summary.push(format!(
            "tiled {name}: mean fill {:.1}% (min {:.1}%), {silent_steps} step(s) silent > LIFE/4; on the {} changed transitions \
             (mean cover {:.0}k chars, open walk mean {:.0}k; unspent closed-level characters flowing to the open walk, mean over all steps {:.0}k): reused mean resume-at-deepest-unchanged {:.0}k (reread {:.1}M), {}, \
             common entry prefix {:.0}k (reread {:.1}M); walk in use for comparison: single prefix reread {:.1}M, common-prefix reread {:.1}M\n    \
             open walk by memory start, mean over all steps: {}\n    whole cover by memory start, mean over all steps: {}\n    \
             closed part (everything before the open slot) changed on {closed_changed} of {} transitions; the open slot rolled over \
             (new open unit or run closed) on {open_rolled}; within one open slot the new open list was NOT an extension of the old \
             (an insertion) on {insertions}; open overflow on {overflow_steps} step(s); memories crossing a closed seam at the last step {crossing_last}; \
             levels present at the last step {}\n    {}",
            fills.iter().sum::<f64>() / fills.len().max(1) as f64,
            fills.iter().copied().fold(f64::INFINITY, f64::min),
            used_v.len(),
            mean(&used_v) / 1e3,
            mean(&open_v) / 1e3,
            mean(&flow_v) / 1e3,
            mean(&resume_v) / 1e3,
            reread(&resume_v),
            ats.join(", "),
            mean(&lcp_v) / 1e3,
            reread(&lcp_v),
            total(&base_used) as f64 / 1e6,
            (total(&base_used) - total(&base_lcp)) as f64 / 1e6,
            mean_split(&open_split_v),
            mean_split(&whole_split_v),
            out_steps.len().saturating_sub(1),
            out_steps.last().map_or(String::new(), |s| s.cuts[c]
                .levels
                .iter()
                .map(|l| format!("{}:{}:{}", l.level, l.tiles, l.used))
                .collect::<Vec<_>>()
                .join(" ")),
            lvls.join("\n    "),
        ));
    }
    lines.push(String::new());
    lines.push("== summary".into());
    lines.extend(summary);
    Ok(lines)
}

fn humanize_unit(level: u32) -> String {
    let secs = 4f64.powi(level as i32) * (SEAL_UNIT_NS as f64 / 1e9);
    if secs < 86_400.0 {
        format!("{:.1} h", secs / 3600.0)
    } else {
        format!("{:.1} d", secs / 86_400.0)
    }
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

    #[test]
    fn a_tile_level_changes_only_with_its_digit_or_a_coarser_change() {
        let (spans, costs, observed) = journal(60);
        let mut previous: Option<TiledCut> = None;
        let mut deep_kept_while_young_moved = 0;
        for hours in (40 * 24..60 * 24).step_by(3) {
            let now = 1_000 * 24 * H + hours * H;
            let vis: Vec<usize> = (0..spans.len()).filter(|&i| observed[i] <= now).collect();
            let s: Vec<_> = vis.iter().map(|&i| spans[i]).collect();
            let c: Vec<_> = vis.iter().map(|&i| costs[i]).collect();
            let o: Vec<_> = vis.iter().map(|&i| observed[i]).collect();
            let mut cut = select_tiled_recollection_cut(
                &s,
                &c,
                &o,
                60_000,
                &TiledOpts {
                    anchor_ns: MOMENT_NS,
                    frozen: true,
                    uniform: false,
                    cap_ns: None,
                    closed_fraction: None,
                    open_append: None,
                    deep_weights: false,
                    runs: None,
                },
                None,
            );
            assert!(cut.used <= 60_000);
            for x in cut.cover.iter_mut() {
                *x = vis[*x];
            }
            if let Some(prev) = previous.take() {
                if prev.top == cut.top {
                    let mut coarser_same = true;
                    for l in (0..=cut.top).rev() {
                        let span = |t: &TiledCut| {
                            t.levels
                                .iter()
                                .find(|x| x.level == l)
                                .map(|x| (x.lower, x.upper))
                        };
                        let same = prev.level_entries(l) == cut.level_entries(l);
                        if coarser_same && span(&prev) == span(&cut) {
                            assert!(
                                same,
                                "level {l} changed without its digit or a coarser level changing"
                            );
                        }
                        coarser_same &= same && span(&prev) == span(&cut);
                    }
                    if cut.top >= 2
                        && prev.level_entries(cut.top) == cut.level_entries(cut.top)
                        && prev.level_entries(0) != cut.level_entries(0)
                    {
                        deep_kept_while_young_moved += 1;
                    }
                }
            }
            previous = Some(cut);
        }
        assert!(deep_kept_while_young_moved > 0);
    }

    #[test]
    fn tile_levels_are_the_base4_digits_of_the_newest_unit() {
        let (spans, costs, observed) = journal(30);
        let cut = select_tiled_recollection_cut(
            &spans,
            &costs,
            &observed,
            200_000,
            &TiledOpts {
                anchor_ns: SEAL_UNIT_NS,
                frozen: true,
                uniform: true,
                cap_ns: None,
                closed_fraction: None,
                open_append: None,
                deep_weights: false,
                runs: None,
            },
            None,
        );
        let n = spans
            .iter()
            .map(|s| s.1)
            .max()
            .unwrap()
            .div_euclid(SEAL_UNIT_NS);
        for lv in &cut.levels {
            assert_eq!(lv.tiles as i128, n / 4i128.pow(lv.level) % 4);
        }
        assert!(cut.used <= 200_000);
    }

    #[test]
    fn anchored_head_is_stable_while_the_anchor_holds() {
        let (spans, costs, observed) = journal(60);
        let mut previous: Option<AnchoredCut> = None;
        let mut held = 0;
        for hours in (40 * 24..60 * 24).step_by(5) {
            let now = 1_000 * 24 * H + hours * H;
            let vis: Vec<usize> = (0..spans.len()).filter(|&i| observed[i] <= now).collect();
            let s: Vec<_> = vis.iter().map(|&i| spans[i]).collect();
            let c: Vec<_> = vis.iter().map(|&i| costs[i]).collect();
            let mut cut = select_anchored_recollection_cut(&s, &c, 20_000, 4);
            assert!(cut.used <= 20_000);
            for x in cut.cover.iter_mut() {
                *x = vis[*x];
            }
            if let Some(prev) = previous.take() {
                if (prev.life_q, prev.now_q) == (cut.life_q, cut.now_q) {
                    held += 1;
                }
            }
            previous = Some(cut);
        }
        assert!(held > 0);
    }

    #[test]
    fn civil_day_arithmetic_round_trips_and_knows_mondays() {
        for z in [-800_000i64, -1, 0, 1, 19_000, 20_000, 20_765, 400_000] {
            let (y, m, d) = civil_from_days(z);
            assert_eq!(days_from_civil(y, m, d), z);
        }
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        // 2026-11-09 is a Monday; 2026-11-08 a Sunday.
        assert_eq!(weekday(days_from_civil(2026, 11, 9)), 0);
        assert_eq!(weekday(days_from_civil(2026, 11, 8)), 6);
    }

    #[test]
    fn calendar_slots_nest_without_gaps() {
        let latest = utc_key(days_from_civil(2026, 11, 8), 12) + 1_234_000_000_000;
        let earliest = utc_key(days_from_civil(2024, 8, 26), 0);
        for weeks in [true, false] {
            let (slots, open) = calendar_slots(earliest, latest, weeks);
            assert!(slots.first().unwrap().1 <= earliest);
            for w in slots.windows(2) {
                assert_eq!(w[0].2, w[1].1, "slots must tile time ({weeks})");
                assert!(w[0].0 >= w[1].0, "coarser first ({weeks})");
            }
            assert_eq!(slots.last().unwrap().2, open);
            assert_eq!(open, utc_key(days_from_civil(2026, 11, 8), 12));
            assert_eq!(slots.iter().filter(|s| s.0 == CAL_HOUR).count(), 12);
        }
        // With weeks: Sunday 8 Nov 2026 is in the week of Monday 2 Nov, whose
        // Thursday (5 Nov) is in November, which starts on Monday 2 Nov.
        let (slots, _) = calendar_slots(earliest, latest, true);
        assert_eq!(slots.iter().filter(|s| s.0 == CAL_WEEK).count(), 0);
        assert_eq!(slots.iter().filter(|s| s.0 == CAL_DAY).count(), 6);
    }

    #[test]
    fn frozen_calendar_level_changes_only_with_its_slots_or_a_coarser_change() {
        let (spans, costs, observed) = journal(60);
        let mut previous: Option<CalendarCut> = None;
        for hours in (40 * 24..60 * 24).step_by(5) {
            let now = 1_000 * 24 * H + hours * H;
            let vis: Vec<usize> = (0..spans.len()).filter(|&i| observed[i] <= now).collect();
            let s: Vec<_> = vis.iter().map(|&i| spans[i]).collect();
            let c: Vec<_> = vis.iter().map(|&i| costs[i]).collect();
            let o: Vec<_> = vis.iter().map(|&i| observed[i]).collect();
            let mut cut =
                select_calendar_recollection_cut(&s, &c, &o, 60_000, MOMENT_NS, true, true);
            assert!(cut.used <= 60_000);
            for x in cut.cover.iter_mut() {
                *x = vis[*x];
            }
            if let Some(prev) = previous.take() {
                if prev.years == cut.years {
                    let mut coarser_same = true;
                    for l in [CAL_YEAR, CAL_MONTH, CAL_WEEK, CAL_DAY, CAL_HOUR] {
                        let span_same = prev.level_span(l) == cut.level_span(l);
                        let same = prev.level_entries(l) == cut.level_entries(l);
                        if coarser_same && span_same {
                            assert!(same, "level {l} changed by content alone under policy a");
                        }
                        coarser_same &= same && span_same;
                    }
                }
            }
            previous = Some(cut);
        }
    }

    #[test]
    fn immediate_policy_admits_a_late_summary_at_once_and_frozen_defers_it() {
        let (mut spans, mut costs, mut observed) = journal(60);
        let latest = spans.iter().map(|s| s.1).max().unwrap();
        let newest_obs = observed.iter().copied().max().unwrap();
        // A summary of day 3 (deep in the past), written after everything else.
        let day3 = (1_000 * 24 + 3 * 24) * H;
        spans.push((
            day3,
            day3 + 24 * H - 1,
            Id::new(u128::to_be_bytes(0x5F00_0000_0000_0000_0000_0000_0000_0001)).unwrap(),
        ));
        costs.push(150);
        observed.push(newest_obs + H);
        let _ = latest;
        let late = spans.len() - 1;
        let pick = |frozen: bool| {
            let opts = TiledOpts {
                anchor_ns: MOMENT_NS,
                frozen,
                uniform: true,
                cap_ns: None,
                closed_fraction: Some(2.0 / 3.0),
                open_append: None,
                deep_weights: false,
                runs: None,
            };
            let cut =
                select_tiled_recollection_cut(&spans, &costs, &observed, 300_000, &opts, None);
            cut.cover[..cut.open_from].contains(&late)
        };
        assert!(pick(false), "policy b renders the late summary in its tile");
        assert!(!pick(true), "policy a defers it to the level's next change");
    }

    #[test]
    fn closed_runs_never_move_when_later_memories_arrive() {
        let (spans, costs, observed) = journal(60);
        let opts = TiledOpts {
            anchor_ns: MOMENT_NS,
            frozen: false,
            uniform: true,
            cap_ns: None,
            closed_fraction: None,
            open_append: None,
            deep_weights: false,
            runs: Some(0.1),
        };
        let mut previous: Option<TiledCut> = None;
        let mut appends = 0;
        for hours in (40 * 24..60 * 24).step_by(2) {
            let now = 1_000 * 24 * H + hours * H;
            let vis: Vec<usize> = (0..spans.len()).filter(|&i| observed[i] <= now).collect();
            let s: Vec<_> = vis.iter().map(|&i| spans[i]).collect();
            let c: Vec<_> = vis.iter().map(|&i| costs[i]).collect();
            let o: Vec<_> = vis.iter().map(|&i| observed[i]).collect();
            let mut cut = select_tiled_recollection_cut(&s, &c, &o, 60_000, &opts, None);
            assert!(cut.used <= 60_000);
            for x in cut.cover.iter_mut() {
                *x = vis[*x];
            }
            if let Some(prev) = previous.take() {
                // Seams only advance; within one open run a step that adds
                // only later-starting memories is a pure append.
                assert!(cut.open_start >= prev.open_start, "a seam moved backwards");
                if prev.open_start == cut.open_start && cut.cover.starts_with(&prev.cover) {
                    appends += 1;
                }
            }
            previous = Some(cut);
        }
        assert!(appends > 0);
    }
}
