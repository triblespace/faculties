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
    /// Characters left to the open walk (residual share plus unspent).
    pub open_space: usize,
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

/// The tiled selector.
///
/// The tiling is the old tiled cover's (wiki "The memory cover", 2026-09-05):
/// an absolute grid of 2^14 s units, tiles quadrupling backwards, and at most
/// three complete tiles per level before the current block of the next level.
/// It is the base-4 counter of `n`, the unit holding the newest memory end:
/// level `l` spans `[floor(n/4^(l+1))*4^(l+1), floor(n/4^l)*4^l)` units, so its
/// tile count is the base-4 digit `l` of `n`. A level changes only when its own
/// digit changes (a tile appended, every `4^l` units) or a deeper level carries
/// (four tiles merging into one of the next level).
///
/// Where the old design gave every tile one grain from a globally fitted
/// detail `m`, which could step and re-render the whole cover, each level here
/// gets a fixed share of the budget from the log-age gradient over the
/// counter's own bands, `ln((A + 4^(l+1)u)/(A + 4^l u)) / ln((A + 4^(top+1)u)/A)`
/// with anchor `A` (the 3 s journal moment, or one unit). The share depends on
/// the level and the counter's height only. A level's allowance (share plus
/// what older levels left unspent) is split evenly among its tiles, oldest
/// first, unspent passing to the next tile. Inside each tile today's greedy
/// walk runs over the tile's own span, frozen at the tile's strike: only
/// memories that START in the tile and were observed by the first observation
/// at which the journal's running maximum end reached the tile's end. What all
/// levels leave unspent, plus the residual share of the open unit, goes to
/// today's walk over the open unit `[n*u, L]`, which may fall back on any
/// memory not yet taken.
///
/// Pure in the spans, their first observation times, the budget and the
/// declared anchor. No previous cover, no wall clock, no completeness floor:
/// a tile with too little support simply passes its allowance on. `late` =
/// `(level, shift)` moves one level's observation cutoffs earlier, for the
/// late-arrival probe; the real selector passes `None`.
pub fn select_tiled_recollection_cut(
    spans: &[(i128, i128, Id)],
    costs: &[usize],
    observed: &[i128],
    budget: usize,
    anchor_ns: i128,
    late: Option<(u32, i128)>,
) -> TiledCut {
    assert_eq!(spans.len(), costs.len());
    assert_eq!(spans.len(), observed.len());
    let mut out = TiledCut {
        cover: Vec::new(),
        levels: Vec::new(),
        top: 0,
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
    let a = anchor_ns.max(1) as f64;
    let u = unit as f64;
    let norm = ((a + 4f64.powi(top as i32 + 1) * u) / a).ln();
    let share = |l: u32| -> usize {
        let hi = a + 4f64.powi(l as i32 + 1) * u;
        let lo = a + 4f64.powi(l as i32) * u;
        ((hi / lo).ln() / norm * budget as f64).floor() as usize
    };

    let mut order: Vec<usize> = (0..spans.len()).collect();
    order.sort_by(|&x, &y| {
        observed[x]
            .cmp(&observed[y])
            .then(spans[x].2.cmp(&spans[y].2))
    });
    // Running lived edge in observation order, for each tile's strike.
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
            let cutoff = strike(tb).saturating_sub(shift);
            let candidate: Vec<bool> = spans
                .iter()
                .zip(observed)
                .map(|(s, &o)| s.0 >= ta && s.0 < tb && o <= cutoff)
                .collect();
            let gradient = DensityGradient::new(tb - ta, tile_space);
            let (cover, used) = walk(
                spans,
                costs,
                &candidate,
                &mut selected,
                ta,
                &gradient,
                tile_space,
            );
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

/// Replay the walk in use beside the tiled selector, one configuration per
/// anchor.
pub fn replay_tiled<B: BlobStoreGet, P: TriblePattern>(
    space: &P,
    ws: &B,
    budget: usize,
    steps: usize,
    step_units: i128,
    anchors: &[(String, i128)],
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
        base: Vec<usize>,
        base_used: usize,
        cuts: Vec<TiledCut>,
        silent: Vec<bool>,
    }
    // probes[config][level] = (distinct level states probed, changes per lag)
    let mut probes = vec![vec![(0usize, [0usize; 3]); MAX_LEVEL]; anchors.len()];
    let mut seen: Vec<Vec<Vec<(i128, i128, usize, u32)>>> =
        vec![vec![Vec::new(); MAX_LEVEL]; anchors.len()];
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
        for (c, (_, anchor)) in anchors.iter().enumerate() {
            let mut cut =
                select_tiled_recollection_cut(&sub, &sub_costs, &sub_obs, budget, *anchor, None);
            silent.push(!silent_life_quarters(&sub, &cut.cover).is_empty());
            for lv in &cut.levels {
                let li = lv.level as usize;
                if li >= MAX_LEVEL {
                    continue;
                }
                let key = (lv.lower, lv.upper, lv.allowance, cut.top);
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
                        *anchor,
                        Some((lv.level, lag)),
                    );
                    if late.level_entries(lv.level) != cut.level_entries(lv.level) {
                        probes[c][li].1[j] += 1;
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
    let base_used: Vec<usize> = changed.iter().map(|&k| out_steps[k].base_used).collect();
    let base_lcp: Vec<usize> = changed
        .iter()
        .map(|&k| lcp_chars(&out_steps[k].base, &out_steps[k - 1].base, &costs))
        .collect();
    let mut summary = vec![format!(
        "walk in use: {} of {} transitions changed the cover; on those, mean cover {:.0}k chars, reused with today's single saved prefix 0, \
         mean common entry prefix {:.0}k chars; mean fill {:.1}%; {} step(s) silent > LIFE/4",
        changed.len(),
        out_steps.len().saturating_sub(1),
        mean(&base_used) / 1e3,
        mean(&base_lcp) / 1e3,
        out_steps.iter().map(|s| pct(s.base_used)).sum::<f64>() / out_steps.len().max(1) as f64,
        base_silent,
    )];

    for (c, (name, _)) in anchors.iter().enumerate() {
        lines.push(String::new());
        lines.push(format!(
            "== tiled, anchor {name}: levels coarsest first as level:tiles:used (* = changed since the previous step); \
             resume = chars reused if the backend could resume at the deepest unchanged level boundary; \
             at-j = chars reused with ONE prefix saved at the young end of level j"
        ));
        lines.push(format!(
            "{:<20} {:>8} {:>6} {:>7} {:>8} {:>8} {:>8} {:>8} {:>8}  levels | open",
            "observed", "used", "fill%", "open", "resume", "at-1", "at-2", "at-3", "lcp"
        ));
        let mut fills = Vec::new();
        let mut silent_steps = 0usize;
        let mut resume_v = Vec::new();
        let mut at_v: Vec<Vec<usize>> = vec![Vec::new(); MAX_LEVEL];
        let mut lcp_v = Vec::new();
        let mut used_v = Vec::new();
        let mut open_v = Vec::new();
        let mut lvl_used: Vec<Vec<usize>> = vec![Vec::new(); MAX_LEVEL];
        let mut lvl_changed = vec![0usize; MAX_LEVEL];
        let mut lvl_own = vec![0usize; MAX_LEVEL];
        let mut max_top = 0u32;
        for (k, s) in out_steps.iter().enumerate() {
            let cut = &s.cuts[c];
            max_top = max_top.max(cut.top);
            fills.push(pct(cut.used));
            if s.silent[c] {
                silent_steps += 1;
            }
            let open_used = cut.used - cut.levels.iter().map(|l| l.used).sum::<usize>();
            let mut cells = Vec::new();
            let mut resume = 0usize;
            let mut at = vec![0usize; MAX_LEVEL];
            let mut lcp = 0usize;
            if k > 0 {
                let prev = &out_steps[k - 1].cuts[c];
                let same_top = prev.top == cut.top;
                let mut intact = same_top;
                // Levels coarsest first; a level absent in both is unchanged.
                let mut unchanged_through: Vec<(u32, bool, usize)> = Vec::new();
                for l in (0..=cut.top).rev() {
                    let cur = cut.levels.iter().find(|x| x.level == l);
                    let old = prev.levels.iter().find(|x| x.level == l);
                    let same = same_top
                        && cut.level_entries(l) == prev.level_entries(l)
                        && cur.map(|x| (x.lower, x.upper)) == old.map(|x| (x.lower, x.upper));
                    let used = cur.map_or(0, |x| x.used);
                    if same && intact {
                        resume += used;
                    } else {
                        intact = false;
                    }
                    if !same {
                        lvl_changed[l as usize] += 1;
                        let deeper_same = unchanged_through.iter().all(|&(_, s, _)| s);
                        if deeper_same {
                            lvl_own[l as usize] += 1;
                        }
                    }
                    unchanged_through.push((l, same, used));
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
                // ONE prefix saved at the young end of level j: reused when
                // every level >= j is unchanged.
                for j in 0..=cut.top as usize {
                    let ok = unchanged_through
                        .iter()
                        .filter(|&&(l, _, _)| l as usize >= j)
                        .all(|&(_, s, _)| s);
                    if ok {
                        at[j] = unchanged_through
                            .iter()
                            .filter(|&&(l, _, _)| l as usize >= j)
                            .map(|&(_, _, u)| u)
                            .sum();
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
            lines.push(format!(
                "{:<20} {:>8} {:>5.1}% {:>7} {:>8} {:>8} {:>8} {:>8} {:>8}  {} | {}{}",
                fmt_key(s.point),
                cut.used,
                pct(cut.used),
                open_used,
                resume,
                at[1],
                at[2],
                at[3],
                lcp,
                cells.join(" "),
                open_used,
                if s.silent[c] { "  SILENT>LIFE/4" } else { "" }
            ));
        }
        let total = |v: &[usize]| v.iter().sum::<usize>();
        let reread = |v: &[usize]| (total(&used_v) - total(v)) as f64 / 1e6;
        let ats: Vec<String> = (0..=max_top as usize)
            .map(|j| {
                format!(
                    "at-{j} {:.0}k (reread {:.1}M)",
                    mean(&at_v[j]) / 1e3,
                    reread(&at_v[j])
                )
            })
            .collect();
        let lvls: Vec<String> = (0..=max_top as usize)
            .rev()
            .map(|l| {
                let p = &probes[c][l];
                format!(
                    "level {l} (tiles {}): mean {:.0}k chars over {} steps present; changed on {} transitions ({} with every coarser level unchanged); \
                     late-arrival probe changes {}/{} level states at 1h, {}/{} at 6h, {}/{} at 24h",
                    humanize_unit(l as u32),
                    mean(&lvl_used[l]) / 1e3,
                    lvl_used[l].len(),
                    lvl_changed[l],
                    lvl_own[l],
                    p.1[0], p.0, p.1[1], p.0, p.1[2], p.0
                )
            })
            .collect();
        summary.push(format!(
            "tiled, anchor {name}: mean fill {:.1}% (min {:.1}%), {silent_steps} step(s) silent > LIFE/4; on the {} changed transitions \
             (mean cover {:.0}k chars, open walk mean {:.0}k): reused mean resume-at-deepest-unchanged {:.0}k (reread {:.1}M), {}, \
             common entry prefix {:.0}k (reread {:.1}M); walk in use for comparison: single prefix reread {:.1}M, common-prefix reread {:.1}M\n    {}",
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
            let mut cut = select_tiled_recollection_cut(&s, &c, &o, 60_000, MOMENT_NS, None);
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
        let cut =
            select_tiled_recollection_cut(&spans, &costs, &observed, 200_000, SEAL_UNIT_NS, None);
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
}
