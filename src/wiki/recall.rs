//! Current Wiki readings selected by the topic of a wake.
use super::*;
use std::collections::HashMap;
use triblespace::core::repo::BlobStoreMeta;
use triblespace_search::portable_bm25::{PortableBM25Blob, PortableBM25View};
use triblespace_search::text_bm25::{Bm25Tokenizer, TextAttributeToBm25};
use triblespace_search::tokens::hash_tokens;

pub(crate) type TopicIndex = PortableBM25View<inlineencodings::Handle<blobencodings::UTF8String>>;

pub(crate) fn topic_index<S>(
    store: &mut S,
    source: Collection<blobencodings::SimpleArchive>,
) -> Result<Collection<PortableBM25Blob>>
where
    S: CollectionStoreExt + SnapshotSource,
    S::Snapshot: BlobStoreGet + CapabilityProofRead,
{
    Ok(store.attach::<PortableBM25Blob>(
        source,
        TextAttributeToBm25 {
            attribute: attrs::content.id(),
            tokenizer: Bm25Tokenizer::Word,
        },
    )?)
}

pub(crate) const HEADING: &str = "\nWiki (about this topic):\n";

// Wake needs a few complete readings. Bound the additional revision lookups
// even when one text belongs to many current or archived entries.
const MAX_MATCHING_TEXTS: usize = 128;
const MAX_HEADS: usize = 24;
const MAX_READINGS: usize = 4;
const MAX_INDEX_HITS: usize = MAX_MATCHING_TEXTS * MAX_READINGS;

#[derive(Default)]
pub(crate) struct TopicReadings {
    pub pages: Vec<(Id, String)>,
    pub charge: usize,
    pub skipped: Vec<String>,
}

impl TopicReadings {
    pub fn select<P: TriblePattern>(
        reader: &PileSnapshot,
        facts: &P,
        latest: &LatestIndex,
        ranked: &[(TextHandle, f32)],
        options: &crate::memory_cover::CoverOpts,
        unreadable: &crate::storage::Unreadable<'_>,
    ) -> Self {
        let mut selected = Self::default();
        let allowance = options.budget_chars / 2;
        if allowance <= HEADING.len() {
            return selected;
        }
        // The personal reader historically accepts every ASCII spelling of
        // "cover". Query those exact text handles; no tag bodies need scanning.
        let mut cover_tags = BTreeSet::new();
        for mask in 0..32 {
            let spelling: String = "cover"
                .bytes()
                .enumerate()
                .map(|(i, c)| {
                    char::from(if mask & (1 << i) == 0 {
                        c
                    } else {
                        c.to_ascii_uppercase()
                    })
                })
                .collect();
            cover_tags.extend(named_tags(facts, &spelling));
        }
        let onboarding = named_tags(facts, "onboarding");
        let start_here = named_tags(facts, "start-here");
        // One standing endorsement is the IDF of a term in half a corpus.
        // Onboarding is a weaker general signal; an explicit starting page
        // within onboarding has both endorsements. A rare term can beat either.
        let prior = std::f32::consts::LN_2;
        let standing_heads = tagged_heads(facts, latest, &cover_tags);
        let onboarding_heads = tagged_heads(facts, latest, &onboarding);
        let starting_heads = tagged_heads(facts, latest, &start_here);
        let mut scores: BTreeMap<_, _> =
            standing_heads.into_iter().map(|rev| (rev, prior)).collect();
        for rev in onboarding_heads {
            let weight = if starting_heads.contains(&rev) {
                1.5
            } else {
                0.5
            };
            *scores.entry(rev).or_default() += weight * prior;
        }
        let mut matched = BTreeMap::<Id, f32>::new();
        let mut texts = 0;
        let mut more_matches = ranked.len() > MAX_INDEX_HITS;
        for &(body, score) in ranked.iter().take(MAX_INDEX_HITS) {
            if texts == MAX_MATCHING_TEXTS {
                more_matches = true;
                break;
            }
            let mut current = false;
            for rev in find!(rev: Id, and!(
                latest.has(rev), pattern!(facts, [{ ?rev @ attrs::content: body }])
            )) {
                if active(facts, rev) {
                    current = true;
                    matched.entry(rev).or_insert(score);
                }
            }
            texts += usize::from(current);
        }
        for (revision, score) in matched {
            *scores.entry(revision).or_default() += score;
        }
        // At equal evidence prefer the shorter reading. A title's handle is
        // stable over body-only edits, unlike the revision identity.
        let mut candidates = Vec::new();
        for (rev, score) in scores {
            if !active(facts, rev) {
                continue;
            }
            let mut bytes = 0u64;
            let bodies: BTreeSet<_> =
                find!(body: TextHandle, pattern!(facts, [{ rev @ attrs::content: ?body }]))
                    .collect();
            for &body in &bodies {
                bytes = bytes.saturating_add(
                    reader
                        .metadata(body)
                        .ok()
                        .flatten()
                        .map_or(u64::MAX, |m| m.length),
                );
            }
            let title =
                find!(title: TextHandle, pattern!(facts, [{ rev @ attrs::title: ?title }])).min();
            candidates.push((score, bytes, title, rev, bodies));
        }
        candidates.sort_unstable_by(|a, b| {
            b.0.total_cmp(&a.0)
                .then_with(|| (a.1, a.2, a.3).cmp(&(b.1, b.2, b.3)))
        });
        let mut seen = BTreeSet::new();
        let mut opened = 0;
        for (_, bytes, _, revision, bodies) in candidates {
            let left = allowance - selected.charge;
            if bodies.iter().all(|body| seen.contains(body)) {
                continue;
            }
            // UTF-8 uses at most four bytes per scalar. For an intermediate
            // size inspect only enough characters to prove the body won't fit.
            if bytes != u64::MAX && bytes > left as u64 {
                if bytes > (left as u64).saturating_mul(4) {
                    continue;
                }
                if bodies.iter().any(|&body| {
                    reader
                        .get::<View<str>, _>(body)
                        .is_ok_and(|text| text.chars().take(left.saturating_add(1)).count() > left)
                }) {
                    continue;
                }
            }
            if opened == MAX_HEADS {
                selected.skipped.push(format!(
                    "Wiki recall stopped after opening {opened} candidate revisions"
                ));
                break;
            }
            opened += 1;
            let show = match operations::render_show(facts, latest, reader, revision, false) {
                Ok(show) => show,
                Err(error) => {
                    let reason = unreadable(&error).unwrap_or_else(|| format!("{error:#}"));
                    selected
                        .skipped
                        .push(format!("Wiki topic revision {revision:x}: {reason}"));
                    continue;
                }
            };
            // Include the section heading once, the newline between readings,
            // and the consumer's per-pair overhead. Never clip a revision.
            let framing = if selected.pages.is_empty() {
                HEADING.len()
            } else {
                1
            };
            let charge = show
                .chars()
                .count()
                .saturating_add(framing)
                .saturating_add(options.chunk_overhead);
            if charge <= allowance.saturating_sub(selected.charge) {
                selected.charge += charge;
                selected.pages.push((revision, show));
                seen.extend(bodies);
                if selected.pages.len() == MAX_READINGS {
                    break;
                }
            }
        }
        if more_matches && selected.pages.len() < MAX_READINGS {
            selected.skipped.push(format!(
                "Wiki recall considered up to {MAX_INDEX_HITS} ranked texts and {MAX_MATCHING_TEXTS} matching current texts"
            ));
        }
        selected
    }
}

/// Score a body by its closest indexed window. Recheck revision visibility
/// and read admission at rendering time just as for lexical matches.
pub(crate) fn semantic_bodies(cosines: &[(TextHandle, f32)]) -> Vec<(TextHandle, f32)> {
    // Cosines have a narrower useful range than BM25. Give a standing
    // endorsement one quarter of its lexical weight on this scale.
    const SEMANTIC_WEIGHT: f32 = 4.0;
    let mut ranked: Vec<_> = cosines
        .iter()
        .filter(|(_, c)| c.is_finite() && *c > 0.0)
        .map(|(body, cosine)| {
            (
                *body,
                -SEMANTIC_WEIGHT * (1.0 - cosine.min(1.0 - f32::EPSILON)).ln(),
            )
        })
        .collect();
    ranked.sort_unstable_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    ranked
}

fn named_tags<P: TriblePattern>(facts: &P, name: &str) -> BTreeSet<Id> {
    let name: TextHandle = name.to_owned().to_blob().get_handle();
    find!(tag: Id, pattern!(facts, [{ ?tag @ metadata::name: name }])).collect()
}

/// A tag name can have multiple IDs (or case spellings) after a merge.
/// Each author signal contributes once to a revision.
fn tagged_heads<P: TriblePattern>(
    facts: &P,
    latest: &LatestIndex,
    tags: &BTreeSet<Id>,
) -> BTreeSet<Id> {
    tags.iter()
        .flat_map(|tag| {
            find!(rev: Id, and!(
                latest.has(rev), pattern!(facts, [{ ?rev @ metadata::tag: tag }])
            ))
            .collect::<Vec<_>>()
        })
        .collect()
}

fn active<P: TriblePattern>(facts: &P, revision: Id) -> bool {
    is_revision(facts, revision)
        && !exists!(
            (),
            pattern!(facts, [{ revision @ metadata::tag: &TAG_ARCHIVED_ID }])
        )
}

pub(crate) fn ranked_bodies(index: &TopicIndex, topic: &str) -> Result<Vec<(TextHandle, f32)>> {
    let query = index.query()?;
    let tokens = hash_tokens(topic);
    let mut counts = BTreeMap::new();
    for token in tokens {
        *counts.entry(token).or_insert(0usize) += 1;
    }
    // A single term explicitly names the lookup, including on a tiny Wiki.
    if counts.len() <= 1 {
        return Ok(query.query_multi(&counts.into_keys().collect::<Vec<_>>())?);
    }

    // A sentence contains ordinary instructions as well as subject words.
    // Before adding BM25 to the authored priors, ask whether a body's one
    // or two most selective matching terms narrow the corpus to roughly
    // the number of readings we can show. For two distinct terms, df1*df2/N
    // is an independence estimate; correlated words can still rank a weak
    // match highly. No selected title, topic or language is built in here.
    let mut evidence = HashMap::<TextHandle, (f32, [usize; 2])>::new();
    for (term, count) in counts {
        let postings: Vec<_> = query.query_term(&term)?.collect();
        let frequency = postings.len();
        for (body, score) in postings {
            let (total, [first, second]) = evidence.entry(body).or_insert((0.0, [usize::MAX; 2]));
            *total += score * count as f32;
            if frequency < *first {
                *second = *first;
                *first = frequency;
            } else {
                *second = (*second).min(frequency);
            }
        }
    }
    let cutoff = query.doc_count() as u128 * MAX_READINGS as u128;
    let mut ranked: Vec<_> = evidence
        .into_iter()
        .filter_map(|(body, (score, [first, second]))| {
            (first <= MAX_READINGS || first as u128 * second as u128 <= cutoff)
                .then_some((body, score))
        })
        .collect();
    ranked.sort_unstable_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    Ok(ranked)
}

#[cfg(test)]
mod semantic_ranking_tests {
    use super::*;

    #[test]
    fn endorsement_can_break_a_close_match_while_a_stronger_topic_wins() {
        let plain = "specific reading".to_owned().to_blob().get_handle();
        let standing = "standing reading".to_owned().to_blob().get_handle();
        let outranks_standing = |specific, endorsed| {
            let scores: BTreeMap<_, _> =
                semantic_bodies(&[(plain, specific), (standing, endorsed)])
                    .into_iter()
                    .collect();
            scores[&plain] > scores[&standing] + std::f32::consts::LN_2
        };
        assert!(outranks_standing(0.630, 0.540));
        assert!(!outranks_standing(0.629, 0.572));
    }
}
