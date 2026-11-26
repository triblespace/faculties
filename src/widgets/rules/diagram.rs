//! Geometry of the compact rule-web diagram: a ring of rule marks, directed
//! links offset to their own side (so A → B and B → A stay apart), and small
//! loops for self-edges. Pure functions of positions, so picking an arrow can
//! be checked without a screen. Drawing uses `GORBIE::graph`'s marks.

use egui::{pos2, vec2, Align2, Pos2, Vec2};

/// Sideways offset of a directed link from the centre line.
pub const SIDE: f32 = 4.0;
/// Radius of a self-edge loop.
pub const LOOP: f32 = 8.0;
/// Pointer distance that counts as touching a link.
pub const REACH: f32 = 6.0;
/// How far a node's label sits from its mark, outward.
pub const LABEL: f32 = 30.0;

/// One drawn edge, in screen points.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Mark {
    Link { from: Pos2, to: Pos2 },
    Loop { centre: Pos2 },
}

/// Ring radius that keeps neighbouring marks apart for `count` nodes.
pub fn radius_for(count: usize) -> f32 {
    (count as f32 * 2.2 * GORBIE::graph::CLEARANCE / std::f32::consts::TAU).max(70.0)
}

/// `count` positions on a ring, the first at the top, clockwise. One node
/// sits at the centre.
pub fn ring(count: usize, centre: Pos2, radius: f32) -> Vec<Pos2> {
    if count == 1 {
        return vec![centre];
    }
    (0..count)
        .map(|k| {
            let angle =
                -std::f32::consts::FRAC_PI_2 + std::f32::consts::TAU * k as f32 / count as f32;
            pos2(
                centre.x + radius * angle.cos(),
                centre.y + radius * angle.sin(),
            )
        })
        .collect()
}

/// Unit direction from the ring's centre to a node (up for the centre).
pub fn outward(node: Pos2, centre: Pos2) -> Vec2 {
    let delta = node - centre;
    if delta.length() < 1.0 {
        vec2(0.0, -1.0)
    } else {
        delta.normalized()
    }
}

/// A directed link moved to its own right-hand side.
pub fn link(from: Pos2, to: Pos2) -> Mark {
    let delta = to - from;
    let across = if delta.length() < f32::EPSILON {
        Vec2::ZERO
    } else {
        let along = delta.normalized();
        vec2(-along.y, along.x) * SIDE
    };
    Mark::Link {
        from: from + across,
        to: to + across,
    }
}

/// A self-edge loop just outside the node's mark.
pub fn self_loop(node: Pos2, centre: Pos2) -> Mark {
    Mark::Loop {
        centre: node + outward(node, centre) * (GORBIE::graph::MARK + LOOP + 2.0),
    }
}

/// Where a node's label goes, and how it is anchored there.
pub fn label_at(node: Pos2, centre: Pos2) -> (Pos2, Align2) {
    let out = outward(node, centre);
    let anchor = if out.x > 0.3 {
        Align2::LEFT_CENTER
    } else if out.x < -0.3 {
        Align2::RIGHT_CENTER
    } else if out.y < 0.0 {
        Align2::CENTER_BOTTOM
    } else {
        Align2::CENTER_TOP
    };
    (node + out * LABEL, anchor)
}

fn segment_distance(a: Pos2, b: Pos2, p: Pos2) -> f32 {
    let ab = b - a;
    let length = ab.length_sq();
    if length < f32::EPSILON {
        return (p - a).length();
    }
    let t = ((p - a).dot(ab) / length).clamp(0.0, 1.0);
    (p - (a + ab * t)).length()
}

/// The mark nearest to `at` within `reach`, if any.
pub fn pick(marks: &[Mark], at: Pos2, reach: f32) -> Option<usize> {
    marks
        .iter()
        .enumerate()
        .map(|(index, mark)| {
            let distance = match *mark {
                Mark::Link { from, to } => segment_distance(from, to, at),
                Mark::Loop { centre } => ((at - centre).length() - LOOP).abs(),
            };
            (index, distance)
        })
        .filter(|(_, distance)| *distance <= reach)
        .min_by(|left, right| left.1.total_cmp(&right.1))
        .map(|(index, _)| index)
}
