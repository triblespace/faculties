//! Where a zooid's own history ends, so its next local run can continue it.
//!
//! A native runtime annotates each context it lives with the zooid's name
//! (`context::zooid`) and each occurrence with the context it was observed in
//! (`context::in_context`). A zooid's continuation is read from those alone:
//! its contexts, the nodes observed in each, the one node of each that no
//! other node of that context follows, and of those the newest, the one no
//! later context of the same zooid continues or descends from. A context
//! continues a tail either as a whole (`context::continues`, a zooid picked
//! up by another process) or through its opening node (`node::continues`, a
//! replacement window in the same process).
//!
//! Nothing is guessed. A context with no node, a context with two tails, or
//! two contexts whose tails neither continue nor descend from each other
//! refuses, naming them: a resume from the wrong one would wake the zooid
//! into someone else's moment, or into an older one of its own.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{bail, Result};
use triblespace::prelude::blobencodings::UTF8String;
use triblespace::prelude::*;

use crate::schemas::archive_v3::{context, node, occurrence, witness};

/// The archived node the zooid `name`'s history ends at, or `None` when no
/// context names it.
pub fn zooid_tail<P: TriblePattern>(facts: &P, name: &str) -> Result<Option<Id>> {
    let handle: Inline<triblespace::prelude::inlineencodings::Handle<UTF8String>> =
        name.to_owned().to_blob().get_handle();
    let contexts: BTreeSet<Id> = find!(
        context: Id,
        pattern!(facts, [{ ?context @ context::zooid: handle }])
    )
    .collect();
    if contexts.is_empty() {
        return Ok(None);
    }

    // Each context's one tail, and the tails its nodes say they continue (a
    // replacement window's opening node carries `node::continues`).
    let mut tails: BTreeMap<Id, Id> = BTreeMap::new();
    let mut continued: BTreeSet<Id> = BTreeSet::new();
    let mut empty = Vec::new();
    let mut forked = Vec::new();
    for &ctx in &contexts {
        let nodes: BTreeSet<Id> = find!(
            node: Id,
            pattern!(facts, [
                { _?occurrence @ occurrence::of: _?witness, context::in_context: ctx },
                { _?witness @ witness::projects_to: ?node }
            ])
        )
        .collect();
        if nodes.is_empty() {
            empty.push(ctx);
            continue;
        }
        for &held in &nodes {
            continued.extend(find!(
                tail: Id,
                pattern!(facts, [{ held @ node::continues: ?tail }])
            ));
        }
        let followed: BTreeSet<Id> = nodes
            .iter()
            .flat_map(|&later| predecessors(facts, later))
            .collect();
        let ends: Vec<Id> = nodes.difference(&followed).copied().collect();
        match ends.as_slice() {
            [tail] => {
                tails.insert(ctx, *tail);
            }
            _ => forked.push(format!("context {ctx:x} ends at {}", hexes(&ends))),
        }
    }
    if !empty.is_empty() {
        bail!(
            "zooid {name}: context(s) {} name it but hold no archived node",
            hexes(&empty)
        );
    }
    if !forked.is_empty() {
        bail!("zooid {name}: {}", forked.join("; "));
    }

    // A context is behind when another of the zooid's contexts continues its
    // tail (a resumed process: `context::continues`; a replacement window:
    // `node::continues` on its opening node), or ends at a node descending
    // from it. Contexts that share a tail are one history.
    let distinct: BTreeSet<Id> = tails.values().copied().collect();
    for &ctx in &contexts {
        continued.extend(find!(
            tail: Id,
            pattern!(facts, [{ ctx @ context::continues: ?tail }])
        ));
    }
    let newest: Vec<Id> = distinct
        .iter()
        .copied()
        .filter(|&tail| {
            !continued.contains(&tail)
                && !distinct
                    .iter()
                    .any(|&other| other != tail && ancestors(facts, other).contains(&tail))
        })
        .collect();
    match newest.as_slice() {
        [tail] => Ok(Some(*tail)),
        [] => bail!(
            "zooid {name}: every tail ({}) is continued by another of its contexts; \
             its history has no end",
            hexes(&distinct.into_iter().collect::<Vec<_>>())
        ),
        _ => {
            let by_tail: Vec<String> = newest
                .iter()
                .map(|tail| {
                    let ctxs: Vec<Id> = tails
                        .iter()
                        .filter(|(_, t)| *t == tail)
                        .map(|(ctx, _)| *ctx)
                        .collect();
                    format!("{tail:x} (context {})", hexes(&ctxs))
                })
                .collect();
            bail!(
                "zooid {name}: its history diverges into tails {}; none continues the others",
                by_tail.join(", ")
            )
        }
    }
}

fn predecessors<P: TriblePattern>(facts: &P, later: Id) -> Vec<Id> {
    find!(earlier: Id, pattern!(facts, [{ later @ node::previous: ?earlier }])).collect()
}

/// Every node `tail` descends from, through `node::previous`.
fn ancestors<P: TriblePattern>(facts: &P, tail: Id) -> BTreeSet<Id> {
    let mut seen = BTreeSet::new();
    let mut frontier = vec![tail];
    while let Some(next) = frontier.pop() {
        for earlier in predecessors(facts, next) {
            if seen.insert(earlier) {
                frontier.push(earlier);
            }
        }
    }
    seen
}

fn hexes(ids: &[Id]) -> String {
    ids.iter()
        .map(|id| format!("{id:x}"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use triblespace::core::id::ExclusiveId;
    use triblespace::macros::id_hex;

    const SOURCE: Id = id_hex!("0E6F3C2B8D7A41C59F0B2E7D6A5C4B31");

    /// A scratch archive: chains of nodes, each observed once in a context.
    #[derive(Default)]
    pub(crate) struct Archive {
        pub(crate) facts: Fragment,
        count: usize,
    }

    impl Archive {
        /// One more observation of an existing `node` in `ctx`, as a context
        /// that re-presents the same moment converges on the same node and
        /// occurrence.
        pub(crate) fn observe(&mut self, node: Id, ctx: Id) {
            let witness = crate::archive_v3::witness(SOURCE, node).unwrap();
            let witness_id = witness.root().unwrap();
            let occurrence = crate::archive_v3::occurrence(witness_id, Default::default()).unwrap();
            let occurrence_id = occurrence.root().unwrap();
            self.facts += entity! { ExclusiveId::force_ref(&occurrence_id) @
            context::in_context: ctx };
        }

        /// A node after `previous`, observed in `ctx`.
        pub(crate) fn node(&mut self, previous: Option<Id>, ctx: Id) -> Id {
            self.count += 1;
            let node = crate::archive_v3::node(
                previous,
                crate::archive_v3::sense(&format!("moment {}", self.count)),
            )
            .unwrap();
            let node_id = node.root().unwrap();
            let witness = crate::archive_v3::witness(SOURCE, node_id).unwrap();
            let witness_id = witness.root().unwrap();
            let occurrence = crate::archive_v3::occurrence(witness_id, Default::default()).unwrap();
            let occurrence_id = occurrence.root().unwrap();
            self.facts += node;
            self.facts += witness;
            self.facts += occurrence;
            self.facts += entity! { ExclusiveId::force_ref(&occurrence_id) @
            context::in_context: ctx };
            node_id
        }

        /// `len` nodes in a row from `previous`; the last one.
        pub(crate) fn chain(&mut self, previous: Option<Id>, ctx: Id, len: usize) -> Id {
            (0..len)
                .fold(previous, |tail, _| Some(self.node(tail, ctx)))
                .unwrap()
        }

        pub(crate) fn named(&mut self, ctx: Id, name: &str) {
            let handle = self.facts.put::<UTF8String, _>(name.to_owned());
            self.facts += entity! { ExclusiveId::force_ref(&ctx) @ context::zooid: handle };
        }

        pub(crate) fn continues(&mut self, ctx: Id, tail: Id) {
            self.facts += entity! { ExclusiveId::force_ref(&ctx) @ context::continues: tail };
        }

        pub(crate) fn tail(&self, name: &str) -> Result<Option<Id>> {
            zooid_tail(&TribleSet::from(self.facts.clone()), name)
        }
    }

    pub(crate) fn ctx() -> Id {
        *triblespace::core::id::fucid()
    }

    #[test]
    fn a_zooid_continues_its_newest_tail_and_never_another_zooids() {
        let mut archive = Archive::default();
        // A lives twice: its second context continues the first's tail with
        // a fresh chain, as a resumed Drive epoch does.
        let (a1, a2) = (ctx(), ctx());
        archive.named(a1, "zooid-a");
        let first = archive.chain(None, a1, 3);
        archive.named(a2, "zooid-a");
        archive.continues(a2, first);
        let second = archive.chain(None, a2, 4);
        // B lives once, later, and its chain is longer.
        let b = ctx();
        archive.named(b, "zooid-b");
        let b_tail = archive.chain(None, b, 6);
        // A context nobody named.
        let anonymous = ctx();
        archive.chain(None, anonymous, 2);

        assert_eq!(archive.tail("zooid-a").unwrap(), Some(second));
        assert_eq!(archive.tail("zooid-b").unwrap(), Some(b_tail));
        assert_eq!(archive.tail("zooid-unknown").unwrap(), None);
        // A name is matched exactly, not by prefix.
        assert_eq!(archive.tail("zooid").unwrap(), None);
    }

    #[test]
    fn a_tail_that_descends_from_another_contexts_is_the_newer_one() {
        // Two contexts that share a prefix of nodes (a re-presented system
        // body converges on the same nodes) and the later one goes further.
        let mut archive = Archive::default();
        let (early, late) = (ctx(), ctx());
        archive.named(early, "zooid-a");
        archive.named(late, "zooid-a");
        let root = archive.node(None, early);
        let shared = archive.node(Some(root), early);
        archive.observe(root, late);
        archive.observe(shared, late);
        let tail = archive.chain(Some(shared), late, 2);
        assert_eq!(archive.tail("zooid-a").unwrap(), Some(tail));
    }

    #[test]
    fn a_replacement_window_continues_through_its_opening_node() {
        // In one process a window is replaced: the new context opens at a
        // fresh root whose node::continues names the old tail.
        let mut archive = Archive::default();
        let (old, new) = (ctx(), ctx());
        archive.named(old, "zooid-a");
        let before = archive.chain(None, old, 3);
        archive.named(new, "zooid-a");
        let opens = archive.node(None, new);
        archive.facts += crate::archive_v3::annotate_node(
            opens,
            &crate::archive_v3::NodeAnnotations {
                continues: vec![before],
                ..Default::default()
            },
        );
        let after = archive.chain(Some(opens), new, 2);
        assert_eq!(archive.tail("zooid-a").unwrap(), Some(after));
    }

    #[test]
    fn divergent_tails_refuse_naming_both() {
        let mut archive = Archive::default();
        let (one, two) = (ctx(), ctx());
        archive.named(one, "zooid-a");
        archive.named(two, "zooid-a");
        let left = archive.chain(None, one, 2);
        let right = archive.chain(None, two, 2);
        let error = format!("{:#}", archive.tail("zooid-a").unwrap_err());
        assert!(error.contains("diverges"), "{error}");
        assert!(error.contains(&format!("{left:x}")), "{error}");
        assert!(error.contains(&format!("{right:x}")), "{error}");
    }

    #[test]
    fn a_context_continued_from_before_its_tail_is_a_divergence() {
        // A second context resumes from the middle of the first: the first's
        // later turns and the second's are two histories.
        let mut archive = Archive::default();
        let (one, two) = (ctx(), ctx());
        archive.named(one, "zooid-a");
        let middle = archive.chain(None, one, 2);
        archive.chain(Some(middle), one, 2);
        archive.named(two, "zooid-a");
        archive.continues(two, middle);
        archive.chain(None, two, 2);
        let error = format!("{:#}", archive.tail("zooid-a").unwrap_err());
        assert!(error.contains("diverges"), "{error}");
    }

    #[test]
    fn a_context_with_two_tails_or_no_node_refuses_naming_it() {
        let mut archive = Archive::default();
        let forked = ctx();
        archive.named(forked, "zooid-a");
        let root = archive.node(None, forked);
        let left = archive.node(Some(root), forked);
        let right = archive.node(Some(root), forked);
        let error = format!("{:#}", archive.tail("zooid-a").unwrap_err());
        assert!(
            error.contains(&format!("context {forked:x} ends at")),
            "{error}"
        );
        assert!(error.contains(&format!("{left:x}")) && error.contains(&format!("{right:x}")));

        let mut archive = Archive::default();
        let hollow = ctx();
        archive.named(hollow, "zooid-b");
        let error = format!("{:#}", archive.tail("zooid-b").unwrap_err());
        assert!(error.contains(&format!("{hollow:x}")), "{error}");
        assert!(error.contains("hold no archived node"), "{error}");
    }
}
