//! A static rule web: which rule's conclusions could feed which rule's matches.
//!
//! In the spirit of precomputed construction overlaps (Streamlining Formal
//! CxG): for an ordered pair of revisions A → B, an *overlap* assigns some of
//! B's `matches` clauses to A's `core`/`emits` clauses with the same predicate,
//! such that all implied term equalities are jointly consistent (literal
//! alternatives on a merged term must intersect). Only overlaps that cannot be
//! extended by another consistent assignment are kept (maximal). B's matches
//! outside the overlap are its remaining conditions.
//!
//! This is analysis of program facts, nothing more:
//! - an edge says A *can* produce a fact B matches, not that it did;
//! - a cycle is a static feedback cycle. Firings are not recorded in the pile,
//!   so observed firings are not shown, and no cycle is claimed productive;
//! - the one termination statement made is the sound Datalog one: a cycle in
//!   which no rule constructs a new identity can only relate values already
//!   present, so it settles whenever its inputs are finite. A cycle through a
//!   constructing rule may settle or keep growing; that is not decided here.
//!
//! Search is bounded per pair; a bounded pair says so.

use super::projection::{ClauseKind, RuleLine, Value};
use triblespace::core::inline::RawInline;

/// Revisions considered for the web.
pub const MAX_WEB_RULES: usize = 64;
/// Search nodes per ordered pair.
pub const MAX_NODES: usize = 4_096;
/// Maximal overlaps kept per ordered pair.
pub const MAX_OVERLAPS: usize = 16;

/// Terms of the producing rule (A) and the consuming rule (B) made equal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Binding {
    pub producer: Vec<usize>,
    pub consumer: Vec<usize>,
    /// The literal alternatives the merged term is still allowed; empty when
    /// no member is a literal.
    pub literals: Vec<Value>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Overlap {
    /// (consumer matches clause, producer output clause), clause indices.
    pub pairs: Vec<(usize, usize)>,
    pub bindings: Vec<Binding>,
    /// Consumer matches clauses this overlap leaves to other facts.
    pub remaining: Vec<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edge {
    /// Indices into the projection's rules.
    pub from: usize,
    pub to: usize,
    pub overlaps: Vec<Overlap>,
    /// The search or the overlap list hit its bound.
    pub bounded: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cycle {
    pub rules: Vec<usize>,
    /// Some rule in the cycle constructs a new identity (has `core` clauses).
    pub constructs: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Web {
    /// Which rules the web covers (indices into the projection's rules).
    pub considered: Vec<usize>,
    /// Whether `considered` is the selected revisions (else all listed).
    pub selected_only: bool,
    pub truncated: bool,
    pub edges: Vec<Edge>,
    pub cycles: Vec<Cycle>,
}

impl Web {
    pub fn feeds(&self, rule: usize) -> impl Iterator<Item = &Edge> {
        self.edges.iter().filter(move |edge| edge.from == rule)
    }

    pub fn fed_by(&self, rule: usize) -> impl Iterator<Item = &Edge> {
        self.edges.iter().filter(move |edge| edge.to == rule)
    }

    pub fn cycles_through(&self, rule: usize) -> impl Iterator<Item = &Cycle> {
        self.cycles
            .iter()
            .filter(move |cycle| cycle.rules.contains(&rule))
    }
}

/// Union-find over the producer's terms (0..n) and the consumer's (n..).
/// Each class keeps the literal alternatives all its literal members allow.
#[derive(Clone)]
struct Classes {
    parent: Vec<usize>,
    /// `None`: unconstrained; `Some(set)`: allowed values (never empty).
    allowed: Vec<Option<Vec<RawInline>>>,
}

impl Classes {
    fn new(producer: &RuleLine, consumer: &RuleLine) -> Self {
        let allowed = producer
            .terms
            .iter()
            .chain(&consumer.terms)
            .map(|term| {
                (!term.literals.is_empty())
                    .then(|| term.literals.iter().map(|value| value.raw).collect())
            })
            .collect::<Vec<_>>();
        Self {
            parent: (0..allowed.len()).collect(),
            allowed,
        }
    }

    fn find(&mut self, node: usize) -> usize {
        let mut root = node;
        while self.parent[root] != root {
            root = self.parent[root];
        }
        let mut node = node;
        while self.parent[node] != root {
            let next = self.parent[node];
            self.parent[node] = root;
            node = next;
        }
        root
    }

    /// Merge two terms; false when their literal alternatives are disjoint.
    fn union(&mut self, a: usize, b: usize) -> bool {
        let (a, b) = (self.find(a), self.find(b));
        if a == b {
            return true;
        }
        let allowed = match (self.allowed[a].take(), self.allowed[b].take()) {
            (None, other) | (other, None) => other,
            (Some(left), Some(right)) => {
                let both: Vec<_> = left.into_iter().filter(|v| right.contains(v)).collect();
                if both.is_empty() {
                    return false;
                }
                Some(both)
            }
        };
        self.parent[b] = a;
        self.allowed[a] = allowed;
        true
    }
}

struct Search<'r> {
    producer: &'r RuleLine,
    consumer: &'r RuleLine,
    offset: usize,
    /// Consumer matches clauses, and for each its candidate producer outputs.
    matches: Vec<(usize, Vec<usize>)>,
    nodes: usize,
    bounded: bool,
    found: Vec<(Vec<(usize, usize)>, Classes)>,
}

impl Search<'_> {
    fn unify(&self, classes: &mut Classes, matched: usize, output: usize) -> bool {
        let m = &self.consumer.clauses[matched];
        let o = &self.producer.clauses[output];
        classes.union(o.subject, self.offset + m.subject)
            && classes.union(o.object, self.offset + m.object)
    }

    fn walk(&mut self, at: usize, chosen: &mut Vec<(usize, usize)>, classes: Classes) {
        if self.bounded {
            return;
        }
        self.nodes += 1;
        if self.nodes > MAX_NODES || self.found.len() >= MAX_OVERLAPS {
            self.bounded = true;
            return;
        }
        if at == self.matches.len() {
            if chosen.is_empty() || !self.maximal(chosen, &classes) {
                return;
            }
            self.found.push((chosen.clone(), classes));
            return;
        }
        let (matched, candidates) = self.matches[at].clone();
        for output in candidates {
            let mut next = classes.clone();
            if self.unify(&mut next, matched, output) {
                chosen.push((matched, output));
                self.walk(at + 1, chosen, next);
                chosen.pop();
            }
        }
        self.walk(at + 1, chosen, classes);
    }

    /// No unassigned consumer clause could still be added consistently.
    fn maximal(&self, chosen: &[(usize, usize)], classes: &Classes) -> bool {
        self.matches.iter().all(|(matched, candidates)| {
            chosen.iter().any(|(m, _)| m == matched)
                || candidates.iter().all(|&output| {
                    let mut probe = classes.clone();
                    !self.unify(&mut probe, *matched, output)
                })
        })
    }
}

fn overlaps(producer: &RuleLine, consumer: &RuleLine) -> (Vec<Overlap>, bool) {
    let outputs: Vec<usize> = producer
        .clauses
        .iter()
        .enumerate()
        .filter(|(_, clause)| clause.kind != ClauseKind::Matches)
        .map(|(index, _)| index)
        .collect();
    let matches: Vec<(usize, Vec<usize>)> = consumer
        .clauses(ClauseKind::Matches)
        .map(|(index, clause)| {
            let candidates = outputs
                .iter()
                .copied()
                .filter(|&output| producer.clauses[output].predicate == clause.predicate)
                .collect();
            (index, candidates)
        })
        .collect();
    if matches.iter().all(|(_, candidates)| candidates.is_empty()) {
        return (Vec::new(), false);
    }
    let mut search = Search {
        producer,
        consumer,
        offset: producer.terms.len(),
        matches,
        nodes: 0,
        bounded: false,
        found: Vec::new(),
    };
    search.walk(0, &mut Vec::new(), Classes::new(producer, consumer));
    let offset = search.offset;
    let all: Vec<usize> = search.matches.iter().map(|(index, _)| *index).collect();
    let found = search
        .found
        .into_iter()
        .map(|(pairs, mut classes)| {
            let mut groups: Vec<(usize, Binding)> = Vec::new();
            for node in 0..classes.parent.len() {
                let root = classes.find(node);
                let at = match groups.iter().position(|(r, _)| *r == root) {
                    Some(at) => at,
                    None => {
                        groups.push((
                            root,
                            Binding {
                                producer: Vec::new(),
                                consumer: Vec::new(),
                                literals: Vec::new(),
                            },
                        ));
                        groups.len() - 1
                    }
                };
                if node < offset {
                    groups[at].1.producer.push(node);
                } else {
                    groups[at].1.consumer.push(node - offset);
                }
            }
            let bindings = groups
                .into_iter()
                .filter(|(_, binding)| !binding.producer.is_empty() && !binding.consumer.is_empty())
                .map(|(root, mut binding)| {
                    let allowed = classes.allowed[root].clone().unwrap_or_default();
                    binding.literals = producer
                        .terms
                        .iter()
                        .chain(&consumer.terms)
                        .flat_map(|term| term.literals.iter())
                        .filter(|value| allowed.contains(&value.raw))
                        .fold(Vec::<Value>::new(), |mut kept, value| {
                            if !kept.iter().any(|k| k.raw == value.raw) {
                                kept.push(value.clone());
                            }
                            kept
                        });
                    binding
                })
                .collect();
            let remaining = all
                .iter()
                .copied()
                .filter(|matched| !pairs.iter().any(|(m, _)| m == matched))
                .collect();
            Overlap {
                pairs,
                bindings,
                remaining,
            }
        })
        .collect();
    (found, search.bounded)
}

/// Strongly connected components with a cycle (size > 1, or a self-edge).
fn cycles(considered: &[usize], edges: &[Edge], rules: &[RuleLine]) -> Vec<Cycle> {
    // Tarjan over the considered rules; scratch state for this call only.
    struct State {
        index: Vec<Option<usize>>,
        low: Vec<usize>,
        on_stack: Vec<bool>,
        stack: Vec<usize>,
        next: usize,
        components: Vec<Vec<usize>>,
    }
    fn visit(node: usize, successors: &[Vec<usize>], state: &mut State) {
        state.index[node] = Some(state.next);
        state.low[node] = state.next;
        state.next += 1;
        state.stack.push(node);
        state.on_stack[node] = true;
        for &next in &successors[node] {
            match state.index[next] {
                None => {
                    visit(next, successors, state);
                    state.low[node] = state.low[node].min(state.low[next]);
                }
                Some(index) if state.on_stack[next] => {
                    state.low[node] = state.low[node].min(index);
                }
                Some(_) => {}
            }
        }
        if Some(state.low[node]) == state.index[node] {
            let mut component = Vec::new();
            while let Some(top) = state.stack.pop() {
                state.on_stack[top] = false;
                component.push(top);
                if top == node {
                    break;
                }
            }
            state.components.push(component);
        }
    }
    let position = |rule: usize| considered.iter().position(|r| *r == rule);
    let mut successors = vec![Vec::new(); considered.len()];
    for edge in edges {
        if let (Some(from), Some(to)) = (position(edge.from), position(edge.to)) {
            successors[from].push(to);
        }
    }
    let mut state = State {
        index: vec![None; considered.len()],
        low: vec![0; considered.len()],
        on_stack: vec![false; considered.len()],
        stack: Vec::new(),
        next: 0,
        components: Vec::new(),
    };
    for node in 0..considered.len() {
        if state.index[node].is_none() {
            visit(node, &successors, &mut state);
        }
    }
    let mut found: Vec<Cycle> = state
        .components
        .into_iter()
        .filter(|component| component.len() > 1 || successors[component[0]].contains(&component[0]))
        .map(|component| {
            let mut members: Vec<usize> = component.into_iter().map(|n| considered[n]).collect();
            members.sort();
            Cycle {
                constructs: members
                    .iter()
                    .any(|&rule| !rules[rule].constructed_terms().is_empty()),
                rules: members,
            }
        })
        .collect();
    found.sort_by(|a, b| a.rules.cmp(&b.rules));
    found
}

/// The web over the selected revisions, or over all listed ones when none is
/// selected (a static what-if, labelled as such by the viewer).
pub fn web(rules: &[RuleLine]) -> Web {
    let selected: Vec<usize> = (0..rules.len())
        .filter(|&index| !rules[index].selected_in.is_empty())
        .collect();
    let selected_only = !selected.is_empty();
    let mut considered = if selected_only {
        selected
    } else {
        (0..rules.len()).collect()
    };
    let truncated = considered.len() > MAX_WEB_RULES;
    considered.truncate(MAX_WEB_RULES);
    let mut edges = Vec::new();
    for &from in &considered {
        for &to in &considered {
            let (overlaps, bounded) = overlaps(&rules[from], &rules[to]);
            if !overlaps.is_empty() || bounded {
                edges.push(Edge {
                    from,
                    to,
                    overlaps,
                    bounded,
                });
            }
        }
    }
    let cycles = cycles(&considered, &edges, rules);
    Web {
        considered,
        selected_only,
        truncated,
        edges,
        cycles,
    }
}
