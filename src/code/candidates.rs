//! Bounded candidate exploration over the existing lexical and mention facts.
//! These are independent retrieval reasons, never an equivalence verdict.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{bail, Result};
use triblespace::prelude::*;

use super::index::{entity_scores, ranked_entities, score_texts, LexicalScores, Tier};
use super::operations::{
    hit, item_source, keeps, placements_of_item, provenance, select_scans, Code, Filter, Hit,
    Provenance, Texts,
};
use super::{Located, SpanValue, TextHandle};
use crate::schemas::code::attrs;

#[derive(Clone, Debug)]
pub struct SharedMention {
    pub handle: TextHandle,
    /// Unknown extractor spellings remain visible by handle.
    pub spelling: Option<String>,
    /// Distinct non-seed items in the selected scope naming this mention.
    pub items: usize,
}

#[derive(Clone, Debug)]
pub struct Candidate {
    pub item: Id,
    pub places: Vec<Hit>,
    pub lexical: LexicalScores,
    pub lexical_rank: Option<usize>,
    pub shared_mentions: Vec<SharedMention>,
    pub mention_rank: Option<usize>,
    /// Source from the first reported placement, with its original line span.
    pub source: Option<String>,
}

#[derive(Clone, Debug)]
pub struct CandidateReport {
    pub seed: Id,
    /// Includes the seed's own occurrences: equal items have no canonical home.
    pub exact_placements: Vec<Hit>,
    pub candidates: Vec<Candidate>,
    /// Each independent channel admits at most this many items; union <= 2*top.
    pub top: usize,
    pub lexical_items: usize,
    pub mention_items: usize,
    pub provenance: Provenance,
}

impl Code {
    /// Explore declarations sharing lexical evidence or syntactic mentions.
    /// Scope applies to candidates and their frequency denominator, not the seed:
    /// a consumer can ask for candidates in a different repository/revision.
    /// No IDs are derived and no corpus model or new index is constructed.
    pub fn candidates(
        &self,
        seed: Id,
        top: usize,
        with_source: bool,
        filter: &Filter,
    ) -> Result<CandidateReport> {
        self.with_operation(|code| code.candidates_scoped(seed, top, with_source, filter))
    }

    fn candidates_scoped(
        &self,
        seed: Id,
        top: usize,
        with_source: bool,
        filter: &Filter,
    ) -> Result<CandidateReport> {
        let targets = self.lexical_targets()?;
        let observed = self.observe()?;
        let facts = crate::storage::acquire_attached_facts(&observed)?;
        let reader = observed.snapshot();
        let mut texts = Texts::new(reader);
        let scans = select_scans(&facts, &mut texts, filter)?;
        let mut query = String::new();
        let mut spellings = BTreeMap::new();
        for tokens in find!(tokens: TextHandle, pattern!(&facts, [
            { seed @ attrs::source_tokens: ?tokens },
        ])) {
            let tokens = texts.get(tokens)?;
            for spelling in super::extract::mention_spellings(&tokens) {
                spellings.insert(super::text_handle(&spelling), spelling);
            }
            query.push_str(&tokens);
            query.push('\n');
        }
        if query.is_empty() {
            bail!("no catalogued source tokens for item {seed:x}");
        }
        for doc in super::item_doc(&facts, seed) {
            query.push_str(&texts.get(doc)?);
            query.push('\n');
        }
        let scores = entity_scores(&facts, &score_texts(reader, targets, &query, Tier::Both)?);
        let mut exact_placements = Vec::new();
        for scan in &scans {
            for located in placements_of_item(&facts, scan.id, seed) {
                let placed = hit(&facts, &mut texts, scan, located)?;
                if keeps(&placed, filter) {
                    exact_placements.push(placed);
                }
            }
        }

        // Only the answers to this question are accumulated, for this call.
        let mut candidates: BTreeMap<Id, Candidate> = BTreeMap::new();
        let mut lexical_order = Vec::new();
        for (item, lexical) in ranked_entities(scores) {
            if item == seed {
                continue;
            }
            let mut places = Vec::new();
            for scan in &scans {
                for located in placements_of_item(&facts, scan.id, item) {
                    let placed = hit(&facts, &mut texts, scan, located)?;
                    if keeps(&placed, filter) {
                        places.push(placed);
                    }
                }
            }
            if places.is_empty() {
                continue;
            }
            lexical_order.push(item);
            candidates.insert(
                item,
                Candidate {
                    item,
                    places,
                    lexical,
                    lexical_rank: Some(lexical_order.len()),
                    shared_mentions: Vec::new(),
                    mention_rank: None,
                    source: None,
                },
            );
        }

        // The seed's stored mention facts drive retrieval. Qualified prefixes,
        // macro bodies and any newer extractor annotations participate unchanged.
        let mut sharing: BTreeMap<TextHandle, BTreeSet<Id>> = BTreeMap::new();
        for scan in &scans {
            for (item, placement, unit, range, mention) in find!(
                (item: Id, placement: Id, unit: Id, range: SpanValue, mention: TextHandle),
                pattern!(&facts, [
                    { seed @ attrs::mentions: ?mention },
                    { ?item @ attrs::mentions: ?mention },
                    { scan.id @ attrs::holds: ?unit },
                    { ?placement @ attrs::unit: ?unit, attrs::item: ?item,
                        attrs::source_range: ?range },
                ])
            ) {
                if item == seed {
                    continue;
                }
                let placed = hit(
                    &facts,
                    &mut texts,
                    scan,
                    Located {
                        item,
                        placement,
                        unit,
                        range,
                    },
                )?;
                if !keeps(&placed, filter) {
                    continue;
                }
                let candidate = candidates.entry(item).or_insert_with(|| Candidate {
                    item,
                    places: Vec::new(),
                    lexical: LexicalScores::default(),
                    lexical_rank: None,
                    shared_mentions: Vec::new(),
                    mention_rank: None,
                    source: None,
                });
                if !candidate.places.contains(&placed) {
                    candidate.places.push(placed);
                }
                sharing.entry(mention).or_default().insert(item);
            }
        }
        let mut mention_scores: BTreeMap<Id, f64> = BTreeMap::new();
        for (handle, items) in sharing {
            let count = items.len();
            for item in items {
                *mention_scores.entry(item).or_default() += 1.0 / count as f64;
                candidates
                    .get_mut(&item)
                    .expect("queried candidate")
                    .shared_mentions
                    .push(SharedMention {
                        handle,
                        spelling: spellings.get(&handle).cloned(),
                        items: count,
                    });
            }
        }
        let mut mention_order: Vec<_> = mention_scores.into_iter().collect();
        mention_order.sort_by(|(left_id, left), (right_id, right)| {
            right.total_cmp(left).then(left_id.cmp(right_id))
        });
        for (rank, (item, _)) in mention_order.iter().enumerate() {
            candidates
                .get_mut(item)
                .expect("queried candidate")
                .mention_rank = Some(rank + 1);
        }
        let mut report = CandidateReport {
            seed,
            exact_placements,
            candidates: Vec::new(),
            top,
            lexical_items: lexical_order.len(),
            mention_items: mention_order.len(),
            provenance: provenance(&facts, &scans),
        };
        // Lexical candidates first, then additional mention candidates. No score
        // fusion: both independent ranks and each selection budget are reported.
        for item in lexical_order
            .into_iter()
            .take(top)
            .chain(mention_order.into_iter().take(top).map(|(item, _)| item))
        {
            let Some(mut candidate) = candidates.remove(&item) else {
                continue;
            };
            candidate.places.sort_by(|left, right| {
                (&left.repo, &left.path, left.line, &left.scan).cmp(&(
                    &right.repo,
                    &right.path,
                    right.line,
                    &right.scan,
                ))
            });
            candidate.places.dedup();
            candidate.shared_mentions.sort_by(|left, right| {
                (left.items, &left.spelling, left.handle).cmp(&(
                    right.items,
                    &right.spelling,
                    right.handle,
                ))
            });
            if with_source {
                candidate.source = item_source(&facts, reader, &candidate.places[0])?;
            }
            report.candidates.push(candidate);
        }
        Ok(report)
    }
}

#[cfg(test)]
#[path = "candidates_tests.rs"]
mod tests;
