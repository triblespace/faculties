//! A pairwise priority game over open Compass goals.
//!
//! The widget shows two open goals (status `todo`, `doing` or `blocked`) and
//! asks which matters more. An answer is handed to the host as a
//! `higher > lower` request; the host performs it as an ordinary Compass
//! priority write (`Compass::prioritize`), with its own storage and identity.
//! "Can't compare" records nothing. The widget reads only the immutable
//! [`DatasetView`] it is given and keeps session-local UI state: the pairs
//! already offered, the current pair, and a small seeded generator.
//!
//! Pairs are chosen by [`choose_pair`]: two open goals with no priority path
//! between them (explicit priorities plus the child-before-parent rule),
//! preferring two in the same priority layer, then goals that take part in
//! no explicit priority yet, with a uniform random choice among the best.

use std::collections::{BTreeSet, HashMap, HashSet};

use triblespace::core::id::Id;
use triblespace::core::inline::encodings::hash::Handle;
use triblespace::core::inline::Inline;
use triblespace::core::metadata;
use triblespace::core::query::TriblePattern;
use triblespace::core::repo::BlobStoreGet;
use triblespace::macros::{find, pattern};
use triblespace::prelude::blobencodings::UTF8String;
use triblespace::prelude::View;
use GORBIE::prelude::CardCtx;

use crate::schemas::compass::{board, latest_status_event, KIND_GOAL_ID, KIND_NOTE_ID};
use crate::widgets::storage::{DatasetRevision, DatasetView};

type TextHandle = Inline<Handle<UTF8String>>;

/// Statuses a goal can be played in.
const OPEN_STATUSES: [&str; 3] = ["todo", "doing", "blocked"];

/// Session-local state of the priority game.
pub struct PriorityGame {
    rng: u64,
    revision: Option<DatasetRevision>,
    /// The pair on screen, in display order.
    pair: Option<(Id, Id)>,
    /// Pairs answered, refused or skipped this session, smaller id first.
    seen: HashSet<(Id, Id)>,
    message: Option<String>,
}

impl Default for PriorityGame {
    fn default() -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos() as u64)
            .unwrap_or(0x5EED);
        Self::with_seed(seed)
    }
}

impl PriorityGame {
    /// A game whose pair choices replay exactly for the same seed and data.
    pub fn with_seed(seed: u64) -> Self {
        Self {
            rng: seed,
            revision: None,
            pair: None,
            seen: HashSet::new(),
            message: None,
        }
    }

    /// Render the game. `prioritize(higher, lower)` hands one answer to the
    /// host; `Err` is a refused write and is shown before the next pair.
    pub fn render(
        &mut self,
        ctx: &mut CardCtx<'_>,
        dataset: DatasetView<'_>,
        mut prioritize: impl FnMut(Id, Id) -> Result<(), String>,
    ) {
        let open = open_goals(dataset);
        if self.revision != Some(dataset.revision) {
            self.revision = Some(dataset.revision);
            let still_open = self.pair.is_some_and(|(a, b)| {
                open.contains(&a)
                    && open.contains(&b)
                    && !comparable(&crate::compass::goal_priority_edges(dataset.facts), a, b)
            });
            if !still_open {
                self.pair = None;
            }
        }
        if self.pair.is_none() {
            self.pair = choose_pair(dataset.facts, &open, &self.seen, &mut self.rng);
        }

        let mut answer: Option<Option<(Id, Id)>> = None;
        ctx.section("Priority game", |ctx| {
            let ui = ctx.ui_mut();
            if let Some(message) = self.message.as_ref() {
                ui.label(egui::RichText::new(message).small().monospace());
            }
            let Some((left, right)) = self.pair else {
                ui.label("No two open goals left to compare.");
                return;
            };
            ui.label(egui::RichText::new("Which matters more?").strong());
            ui.columns(2, |columns| {
                for (column, (goal, other)) in
                    columns.iter_mut().zip([(left, right), (right, left)])
                {
                    goal_card(column, dataset, goal);
                    if column.button("this one").clicked() {
                        answer = Some(Some((goal, other)));
                    }
                }
            });
            if ui.button("can't compare").clicked() {
                answer = Some(None);
            }
        });

        let Some(answer) = answer else {
            return;
        };
        let (left, right) = self.pair.take().expect("answered pair is on screen");
        self.seen.insert(pair_key(left, right));
        self.message = match answer {
            None => Some("Skipped.".to_owned()),
            Some((higher, lower)) => Some(match prioritize(higher, lower) {
                Ok(()) => format!("{} over {}", title(dataset, higher), title(dataset, lower)),
                Err(error) => format!("Not recorded: {error}"),
            }),
        };
        self.pair = choose_pair(dataset.facts, &open, &self.seen, &mut self.rng);
    }
}

/// Goals whose current status, read like the board reads it, is open. A goal
/// without a status event is `todo`.
fn open_goals(dataset: DatasetView<'_>) -> BTreeSet<Id> {
    let Some(register) = dataset.lww_register(board::status_of.id(), metadata::created_at.id())
    else {
        return BTreeSet::new();
    };
    find!(
        goal: Id,
        pattern!(dataset.facts, [{ ?goal @
            metadata::tag: &KIND_GOAL_ID,
            board::title: _?title,
        }])
    )
    .filter(
        |&goal| match latest_status_event(dataset.facts, register, goal) {
            None => true,
            Some((_, status, _)) => dataset
                .reader
                .get::<View<str>, UTF8String>(status)
                .is_ok_and(|status| OPEN_STATUSES.contains(&status.as_ref())),
        },
    )
    .collect()
}

fn pair_key(a: Id, b: Id) -> (Id, Id) {
    if a <= b {
        (a, b)
    } else {
        (b, a)
    }
}

/// splitmix64: small, seedable, and enough for a tie-break.
fn next_random(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Every goal reachable from each of `starts` through `edges`.
fn reachable(edges: &BTreeSet<(Id, Id)>, starts: &BTreeSet<Id>) -> HashMap<Id, HashSet<Id>> {
    let mut lower = HashMap::<Id, Vec<Id>>::new();
    for &(higher, below) in edges {
        lower.entry(higher).or_default().push(below);
    }
    starts
        .iter()
        .map(|&start| {
            let mut seen = HashSet::new();
            let mut pending = lower.get(&start).cloned().unwrap_or_default();
            while let Some(goal) = pending.pop() {
                if seen.insert(goal) {
                    pending.extend(lower.get(&goal).into_iter().flatten().copied());
                }
            }
            (start, seen)
        })
        .collect()
}

/// Whether a priority path joins `a` and `b` in either direction.
fn comparable(edges: &BTreeSet<(Id, Id)>, a: Id, b: Id) -> bool {
    let pair = BTreeSet::from([a, b]);
    let reach = reachable(edges, &pair);
    reach[&a].contains(&b) || reach[&b].contains(&a)
}

/// Choose the next pair of open goals to compare, or `None` when every pair
/// is already ordered or was offered this session.
///
/// Candidates are pairs with no priority path between them under
/// [`crate::compass::goal_priority_edges`]. Among them, a pair in one
/// priority layer ranks first, then the number of its goals that take part in
/// no explicit priority; ties are broken uniformly by `rng`, which also picks
/// the display order.
pub fn choose_pair<P: TriblePattern>(
    facts: &P,
    open: &BTreeSet<Id>,
    seen: &HashSet<(Id, Id)>,
    rng: &mut u64,
) -> Option<(Id, Id)> {
    let edges = crate::compass::goal_priority_edges(facts);
    let ranked: HashSet<Id> = crate::compass::active_priority_edges(facts)
        .into_iter()
        .flat_map(|(higher, lower)| [higher, lower])
        .collect();
    let layers = crate::compass::priority_ranks(open.iter().copied(), &edges);
    let reach = reachable(&edges, open);

    let goals: Vec<Id> = open.iter().copied().collect();
    let mut best: Option<((bool, u8), (Id, Id))> = None;
    let mut ties = 0u64;
    for (index, &a) in goals.iter().enumerate() {
        for &b in &goals[index + 1..] {
            if seen.contains(&pair_key(a, b)) || reach[&a].contains(&b) || reach[&b].contains(&a) {
                continue;
            }
            let score = (
                layers.get(&a) == layers.get(&b),
                u8::from(!ranked.contains(&a)) + u8::from(!ranked.contains(&b)),
            );
            match best {
                Some((current, _)) if score < current => {}
                Some((current, _)) if score == current => {
                    // Reservoir sampling: each tied pair is kept with equal
                    // probability without collecting them.
                    ties += 1;
                    if next_random(rng) % ties == 0 {
                        best = Some((score, (a, b)));
                    }
                }
                _ => {
                    ties = 1;
                    best = Some((score, (a, b)));
                }
            }
        }
    }
    best.map(|(_, (a, b))| {
        if next_random(rng) & 1 == 0 {
            (a, b)
        } else {
            (b, a)
        }
    })
}

fn text(dataset: DatasetView<'_>, handle: TextHandle) -> Option<String> {
    dataset
        .reader
        .get::<View<str>, UTF8String>(handle)
        .ok()
        .map(|text| text.as_ref().to_owned())
}

fn title(dataset: DatasetView<'_>, goal: Id) -> String {
    find!(
        handle: TextHandle,
        pattern!(dataset.facts, [{ goal @
            metadata::tag: &KIND_GOAL_ID,
            board::title: ?handle,
        }])
    )
    .filter_map(|handle| text(dataset, handle))
    .max()
    .unwrap_or_else(|| format!("{goal:x}"))
}

fn goal_card(ui: &mut egui::Ui, dataset: DatasetView<'_>, goal: Id) {
    let facts = dataset.facts;
    let status = dataset
        .lww_register(board::status_of.id(), metadata::created_at.id())
        .and_then(|register| latest_status_event(facts, register, goal))
        .and_then(|(_, status, _)| text(dataset, status))
        .unwrap_or_else(|| "todo".to_owned());
    let mut tags: Vec<String> = find!(
        tag: TextHandle,
        pattern!(facts, [{ goal @
            metadata::tag: &KIND_GOAL_ID,
            board::tag: ?tag,
        }])
    )
    .filter_map(|tag| text(dataset, tag))
    .collect();
    tags.sort();
    tags.dedup();
    let note = find!(
        (note: TextHandle, at: (i128, i128)),
        pattern!(facts, [{ _?event @
            metadata::tag: &KIND_NOTE_ID,
            board::task: &goal,
            board::note: ?note,
            metadata::created_at: ?at,
        }])
    )
    .max_by_key(|(note, at)| (at.0, note.raw))
    .and_then(|(note, _)| text(dataset, note))
    .and_then(|note| {
        note.lines()
            .find(|line| !line.trim().is_empty())
            .map(str::to_owned)
    });

    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(egui::RichText::new(title(dataset, goal)).strong());
        ui.label(
            egui::RichText::new(format!("{status} · {}", &format!("{goal:x}")[..8]))
                .small()
                .monospace(),
        );
        if !tags.is_empty() {
            ui.label(egui::RichText::new(tags.join(" ")).small());
        }
        if let Some(note) = note {
            ui.label(egui::RichText::new(note).small().italics());
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compass::{goal_fragment, priority_snapshot_fragment};
    use hifitime::Epoch;
    use triblespace::prelude::*;

    fn point(seconds: f64) -> Inline<inlineencodings::NsTAIInterval> {
        crate::clock::point(Epoch::from_unix_seconds(seconds)).unwrap()
    }

    fn goal(fragment: &mut Fragment, title: &str, parent: Option<Id>) -> Id {
        let (goal, id) = goal_fragment(title, vec![], parent, point(1.0)).unwrap();
        *fragment += goal;
        id
    }

    /// `higher` ranks over a fresh bottom snapshot of `lower`.
    fn rank(fragment: &mut Fragment, higher: Id, lower: Id) {
        let (bottom, snapshot) = priority_snapshot_fragment(lower, [], point(2.0));
        *fragment += bottom;
        let (over, _) = priority_snapshot_fragment(higher, [snapshot], point(3.0));
        *fragment += over;
    }

    #[test]
    fn ordered_pairs_are_never_offered() {
        let mut fragment = Fragment::empty();
        let a = goal(&mut fragment, "a", None);
        let b = goal(&mut fragment, "b", None);
        let c = goal(&mut fragment, "c", None);
        rank(&mut fragment, a, b);
        // Only (a, c) and (b, c) are unordered.
        let open = BTreeSet::from([a, b, c]);
        let facts = fragment.facts();
        for seed in 0..64 {
            let pair = choose_pair(facts, &open, &HashSet::new(), &mut { seed }).unwrap();
            assert_ne!(pair_key(pair.0, pair.1), pair_key(a, b), "seed {seed}");
        }
    }

    #[test]
    fn unranked_goals_in_one_layer_are_preferred() {
        let mut fragment = Fragment::empty();
        let a = goal(&mut fragment, "a", None);
        let b = goal(&mut fragment, "b", None);
        let c = goal(&mut fragment, "c", None);
        let d = goal(&mut fragment, "d", None);
        rank(&mut fragment, a, b);
        let open = BTreeSet::from([a, b, c, d]);
        let facts = fragment.facts();
        // c and d are both unranked and share a's top layer.
        for seed in 0..64 {
            let pair = choose_pair(facts, &open, &HashSet::new(), &mut { seed }).unwrap();
            assert_eq!(pair_key(pair.0, pair.1), pair_key(c, d), "seed {seed}");
        }
    }

    #[test]
    fn skipped_pairs_and_parent_chains_are_excluded() {
        let mut fragment = Fragment::empty();
        let parent = goal(&mut fragment, "parent", None);
        let child = goal(&mut fragment, "child", Some(parent));
        let other = goal(&mut fragment, "other", None);
        let open = BTreeSet::from([parent, child, other]);
        let facts = fragment.facts();
        let mut seen = HashSet::new();
        let mut rng = 7;
        let mut offered = Vec::new();
        while let Some((a, b)) = choose_pair(facts, &open, &seen, &mut rng) {
            assert!(seen.insert(pair_key(a, b)), "offered twice");
            offered.push(pair_key(a, b));
        }
        offered.sort();
        let mut expected = vec![pair_key(parent, other), pair_key(child, other)];
        expected.sort();
        assert_eq!(offered, expected);
    }

    #[test]
    fn the_same_seed_replays_the_same_choice() {
        let mut fragment = Fragment::empty();
        let open: BTreeSet<Id> = (0..6)
            .map(|index| goal(&mut fragment, &format!("g{index}"), None))
            .collect();
        let facts = fragment.facts();
        let first = choose_pair(facts, &open, &HashSet::new(), &mut 42);
        assert_eq!(first, choose_pair(facts, &open, &HashSet::new(), &mut 42));
    }
}
