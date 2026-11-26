//! A static rule web: which rule's conclusions could feed which rule's matches.
//!
//! In the spirit of precomputed construction overlaps (Streamlining Formal
//! CxG): for an ordered pair of revisions A → B, an *overlap* assigns some of
//! B's `matches` clauses to A's `core`/`emits` clauses with the same predicate,
//! such that all implied term equalities are jointly consistent (literal
//! alternatives on a merged term must intersect). Only overlaps that cannot be
//! extended by another consistent assignment are kept (maximal). B's matches
//! outside the overlap, and all of B's content constraints, are its remaining
//! conditions.
//!
//! This is analysis of program facts, nothing more:
//! - an edge is a *witnessed* consistent assignment: A *can* produce a fact B
//!   matches, structurally, not that it did. A pair whose bounded search
//!   stopped before witnessing one is *undecided*, never an edge;
//! - a web is scoped to what one current reading selects. Revisions selected
//!   by different readings never run together, so they are never combined.
//!   Only when no reading selects anything is a what-if over all listed
//!   revisions drawn, and it is labelled hypothetical;
//! - an edge touching a revision the engine refuses is structural only: that
//!   revision concludes or consumes nothing, so it is kept out of cycles;
//! - a cycle is a static feedback cycle among executable revisions: a
//!   strongly connected component plus one actual closed walk through
//!   witnessed edges. Firings are not recorded in the pile, so observed
//!   firings are not shown, and no cycle is claimed productive;
//! - the one termination statement made is the sound Datalog one: a cycle in
//!   which no rule constructs a new identity can only relate values already
//!   present, so it settles whenever its inputs are finite. It is made only
//!   when the scope's evidence is complete (nothing selected is missing,
//!   unlisted, beyond the bound, partly decoded or undecided), because a lost
//!   rule or pair could join the cycle to a constructing one. A cycle through
//!   a constructing rule may settle or keep growing; that is not decided here.
//!
//! Search is bounded per pair and per web; a bounded result says so.

use super::projection::{ClauseKind, ReadingState, RuleLine, RulesView, Value};
use triblespace::core::inline::RawInline;
use triblespace::prelude::Id;

/// Revisions analysed, across all scopes.
pub const MAX_WEB_RULES: usize = 64;
/// Search nodes per ordered pair.
pub const MAX_NODES: usize = 4_096;
/// Maximal overlaps kept per ordered pair.
pub const MAX_OVERLAPS: usize = 16;
/// Search nodes across every pair of one web.
pub const MAX_TOTAL_NODES: usize = 1 << 20;

/// The bounds of one web, so a caller (or a test) can make them small.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub rules: usize,
    pub nodes: usize,
    pub overlaps: usize,
    pub total_nodes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            rules: MAX_WEB_RULES,
            nodes: MAX_NODES,
            overlaps: MAX_OVERLAPS,
            total_nodes: MAX_TOTAL_NODES,
        }
    }
}

/// What must still hold at runtime for a binding to carry a value across.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Condition {
    /// Several producer terms meet in one class: one firing must bind them
    /// to the same value.
    ProducerEqual,
    /// A literal would still have to equal an identity the producer
    /// constructs at runtime. Structurally compatible; not decided here.
    DerivedLiteral,
    /// The producer's value comes from its own input: only firings whose
    /// value is one of the literals carry across.
    InputLiteral,
    /// Only some of the producer's literal alternatives fit.
    NarrowedLiteral,
}

/// Terms of the producing rule (A) and the consuming rule (B) made equal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Binding {
    pub producer: Vec<usize>,
    pub consumer: Vec<usize>,
    /// The literal alternatives the merged term is still allowed; empty when
    /// no member is a literal.
    pub literals: Vec<Value>,
    pub conditions: Vec<Condition>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Overlap {
    /// (consumer matches clause, producer output clause), clause indices.
    pub pairs: Vec<(usize, usize)>,
    pub bindings: Vec<Binding>,
    /// Consumer matches clauses this overlap leaves to other facts.
    pub remaining: Vec<usize>,
}

impl Overlap {
    /// Nothing is left to other facts or to runtime values: every matches
    /// clause is met by the producer's conclusions, the consumer has no
    /// content constraint, and no binding carries a condition. The producer
    /// must still fire.
    pub fn unconditional(&self, consumer: &RuleLine) -> bool {
        self.remaining.is_empty()
            && consumer.constraints.is_empty()
            && self.bindings.iter().all(|b| b.conditions.is_empty())
    }
}

/// A witnessed edge: at least one consistent assignment was found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edge {
    /// Indices into the projection's rules.
    pub from: usize,
    pub to: usize,
    /// Never empty.
    pub overlaps: Vec<Overlap>,
    /// The search or the overlap list hit its bound: more may exist.
    pub bounded: bool,
    /// Producer or consumer is refused by the engine: structural only.
    pub hypothetical: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unsearched {
    /// The pair's own node bound was reached before any assignment.
    Bound,
    /// The web's total budget ran out before or during this pair.
    Budget,
}

/// A pair whose search ended without a witness and without exhausting its
/// space: neither an edge nor evidence that there is none.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Undecided {
    pub from: usize,
    pub to: usize,
    pub why: Unsearched,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cycle {
    /// The strongly connected component, by rule index: a set, not a walk.
    pub members: Vec<usize>,
    /// One actual closed walk through witnessed edges: `walk[0] → walk[1] →
    /// … → walk[0]`. A single member means a self-edge.
    pub walk: Vec<usize>,
    /// Some rule in the cycle constructs a new identity (has `core` clauses).
    pub constructs: bool,
    /// The settles-on-finite-input statement may be made.
    pub settles: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScopeKind {
    /// What the current reading of these environments selects (several when
    /// their selections are identical).
    Reading(Vec<Id>),
    /// No reading selects anything: all listed revisions, as a what-if no
    /// environment runs.
    WhatIf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scope {
    pub kind: ScopeKind,
    /// Analysed members, by rule index, ascending.
    pub members: Vec<usize>,
    /// Selected revisions with no rule facts in this collection.
    pub missing: usize,
    /// Revisions beyond the projection's listing bound.
    pub unlisted: usize,
    /// Listed revisions beyond the web's rule bound.
    pub beyond: usize,
    /// Members only partly decoded.
    pub partial: Vec<usize>,
    /// Members the engine refuses.
    pub refused: Vec<usize>,
    /// Undecided pairs among members.
    pub undecided: usize,
    pub cycles: Vec<Cycle>,
}

impl Scope {
    fn new(kind: ScopeKind) -> Self {
        Self {
            kind,
            members: Vec::new(),
            missing: 0,
            unlisted: 0,
            beyond: 0,
            partial: Vec::new(),
            refused: Vec::new(),
            undecided: 0,
            cycles: Vec::new(),
        }
    }

    /// Nothing that could change an edge or a cycle was lost or left open.
    pub fn complete(&self) -> bool {
        self.missing == 0
            && self.unlisted == 0
            && self.beyond == 0
            && self.partial.is_empty()
            && self.undecided == 0
    }

    /// Both ends are members.
    pub fn holds(&self, from: usize, to: usize) -> bool {
        self.members.contains(&from) && self.members.contains(&to)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Web {
    /// Rules any scope analyses (indices into the projection's rules).
    pub analysed: Vec<usize>,
    /// Witnessed edges between members of a common scope.
    pub edges: Vec<Edge>,
    pub undecided: Vec<Undecided>,
    pub scopes: Vec<Scope>,
}

impl Web {
    /// Witnessed edges within one scope.
    pub fn edges_in<'w>(&'w self, scope: &'w Scope) -> impl Iterator<Item = &'w Edge> {
        self.edges
            .iter()
            .filter(move |edge| scope.holds(edge.from, edge.to))
    }

    pub fn undecided_in<'w>(&'w self, scope: &'w Scope) -> impl Iterator<Item = &'w Undecided> {
        self.undecided
            .iter()
            .filter(move |pair| scope.holds(pair.from, pair.to))
    }

    /// Edges with `rule` at either end, each once.
    pub fn touching(&self, rule: usize) -> impl Iterator<Item = &Edge> {
        self.edges
            .iter()
            .filter(move |edge| edge.from == rule || edge.to == rule)
    }

    pub fn edge(&self, from: usize, to: usize) -> Option<&Edge> {
        self.edges
            .iter()
            .find(|edge| edge.from == from && edge.to == to)
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
    max_nodes: usize,
    max_overlaps: usize,
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
        if self.nodes > self.max_nodes || self.found.len() >= self.max_overlaps {
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

/// The outcome of searching one ordered pair.
enum Searched {
    /// No consumer clause shares a predicate with a producer output, or the
    /// whole space was searched without a consistent assignment.
    Nothing,
    Found {
        overlaps: Vec<Overlap>,
        bounded: bool,
    },
    Undecided(Unsearched),
}

/// What must hold at runtime for one class to carry a value across.
fn conditions(
    producer: &RuleLine,
    members: &[usize],
    allowed: Option<&Vec<RawInline>>,
) -> Vec<Condition> {
    let mut found = Vec::new();
    if members.len() > 1 {
        found.push(Condition::ProducerEqual);
    }
    if let Some(allowed) = allowed {
        let constructed = producer.constructed_terms();
        for &term in members {
            let literals = &producer.terms[term].literals;
            let condition = if constructed.contains(&term) {
                Some(Condition::DerivedLiteral)
            } else if literals.is_empty() {
                Some(Condition::InputLiteral)
            } else if literals.len() > allowed.len() {
                Some(Condition::NarrowedLiteral)
            } else {
                None
            };
            if let Some(condition) = condition {
                if !found.contains(&condition) {
                    found.push(condition);
                }
            }
        }
    }
    found
}

fn search(
    producer: &RuleLine,
    consumer: &RuleLine,
    limits: Limits,
    budget: &mut usize,
) -> Searched {
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
        return Searched::Nothing;
    }
    if *budget == 0 {
        return Searched::Undecided(Unsearched::Budget);
    }
    let max_nodes = limits.nodes.min(*budget);
    let mut search = Search {
        producer,
        consumer,
        offset: producer.terms.len(),
        matches,
        max_nodes,
        max_overlaps: limits.overlaps,
        nodes: 0,
        bounded: false,
        found: Vec::new(),
    };
    search.walk(0, &mut Vec::new(), Classes::new(producer, consumer));
    *budget -= search.nodes.min(*budget);
    if search.found.is_empty() {
        return if !search.bounded {
            Searched::Nothing
        } else if max_nodes < limits.nodes {
            Searched::Undecided(Unsearched::Budget)
        } else {
            Searched::Undecided(Unsearched::Bound)
        };
    }
    let offset = search.offset;
    let all: Vec<usize> = search.matches.iter().map(|(index, _)| *index).collect();
    let overlaps = search
        .found
        .into_iter()
        .map(|(pairs, mut classes)| {
            // Classes of this one result, grouped by root; scratch only.
            let mut groups: Vec<(usize, Vec<usize>, Vec<usize>)> = Vec::new();
            for node in 0..classes.parent.len() {
                let root = classes.find(node);
                let at = match groups.iter().position(|(r, _, _)| *r == root) {
                    Some(at) => at,
                    None => {
                        groups.push((root, Vec::new(), Vec::new()));
                        groups.len() - 1
                    }
                };
                if node < offset {
                    groups[at].1.push(node);
                } else {
                    groups[at].2.push(node - offset);
                }
            }
            let bindings = groups
                .into_iter()
                .filter(|(_, producer, consumer)| !producer.is_empty() && !consumer.is_empty())
                .map(|(root, members, consumer_terms)| {
                    let allowed = classes.allowed[root].as_ref();
                    let literals = match allowed {
                        None => Vec::new(),
                        Some(allowed) => producer
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
                            }),
                    };
                    Binding {
                        conditions: conditions(producer, &members, allowed),
                        producer: members,
                        consumer: consumer_terms,
                        literals,
                    }
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
    Searched::Found {
        overlaps,
        bounded: search.bounded,
    }
}

/// Shortest closed walk through `start` inside one component, by position.
fn closed_walk(start: usize, component: &[usize], successors: &[Vec<usize>]) -> Vec<usize> {
    if component.len() == 1 {
        return vec![start];
    }
    let mut parent: Vec<Option<usize>> = vec![None; successors.len()];
    let mut seen = vec![false; successors.len()];
    seen[start] = true;
    let mut queue = std::collections::VecDeque::from([start]);
    while let Some(node) = queue.pop_front() {
        for &next in &successors[node] {
            if !component.contains(&next) || (next == start && node == start) {
                continue;
            }
            if next == start {
                let mut walk = vec![node];
                let mut at = node;
                while let Some(previous) = parent[at] {
                    walk.push(previous);
                    at = previous;
                }
                walk.reverse();
                return walk;
            }
            if !seen[next] {
                seen[next] = true;
                parent[next] = Some(node);
                queue.push_back(next);
            }
        }
    }
    // Unreachable for a strongly connected component; say only the set.
    component.to_vec()
}

/// Strongly connected components with a cycle (size > 1, or a self-edge),
/// over `considered` rules and the supported edges among them.
fn cycles(considered: &[usize], edges: &[&Edge], rules: &[RuleLine], complete: bool) -> Vec<Cycle> {
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
        .map(|mut component| {
            component.sort();
            let walk = closed_walk(component[0], &component, &successors);
            let members: Vec<usize> = component.iter().map(|&n| considered[n]).collect();
            let constructs = members
                .iter()
                .any(|&rule| !rules[rule].constructed_terms().is_empty());
            Cycle {
                walk: walk.into_iter().map(|n| considered[n]).collect(),
                settles: !constructs && complete,
                constructs,
                members,
            }
        })
        .collect();
    found.sort_by(|a, b| a.members.cmp(&b.members));
    found
}

/// Admit `rule` to the analysed set unless the bound is reached.
fn admit(rule: usize, analysed: &mut Vec<usize>, bound: usize) -> bool {
    if analysed.contains(&rule) {
        return true;
    }
    if analysed.len() >= bound {
        return false;
    }
    analysed.push(rule);
    true
}

/// The web of each current reading's selection, or, when no reading selects
/// anything, a what-if over all listed revisions (labelled as such).
pub fn web(view: &RulesView) -> Web {
    web_with(view, Limits::default())
}

pub fn web_with(view: &RulesView, limits: Limits) -> Web {
    let rules = &view.rules;
    let position = |id: Id| rules.iter().position(|rule| rule.revision == id);
    // Distinct selections of current readings, in environment order.
    let mut selections: Vec<(Vec<Id>, &Vec<Id>)> = Vec::new();
    for environment in &view.environments {
        if let ReadingState::Sole { selects, .. } = &environment.state {
            if selects.is_empty() {
                continue;
            }
            match selections.iter_mut().find(|(_, s)| *s == selects) {
                Some((anchors, _)) => anchors.push(environment.anchor),
                None => selections.push((vec![environment.anchor], selects)),
            }
        }
    }
    let mut analysed = Vec::new();
    let mut scopes = Vec::new();
    for (anchors, selects) in selections {
        let mut scope = Scope::new(ScopeKind::Reading(anchors));
        for &id in selects {
            match position(id) {
                Some(rule) if admit(rule, &mut analysed, limits.rules) => scope.members.push(rule),
                Some(_) => scope.beyond += 1,
                None if view.selected_missing.contains(&id) => scope.missing += 1,
                None => scope.unlisted += 1,
            }
        }
        scope.members.sort();
        scopes.push(scope);
    }
    if scopes.is_empty() && !rules.is_empty() {
        let mut scope = Scope::new(ScopeKind::WhatIf);
        scope.unlisted = view.revisions_total.saturating_sub(rules.len());
        for rule in 0..rules.len() {
            if admit(rule, &mut analysed, limits.rules) {
                scope.members.push(rule);
            } else {
                scope.beyond += 1;
            }
        }
        scopes.push(scope);
    }

    let mut budget = limits.total_nodes;
    let mut edges = Vec::new();
    let mut undecided = Vec::new();
    for &from in &analysed {
        for &to in &analysed {
            if !scopes.iter().any(|scope| scope.holds(from, to)) {
                continue;
            }
            match search(&rules[from], &rules[to], limits, &mut budget) {
                Searched::Nothing => {}
                Searched::Found { overlaps, bounded } => edges.push(Edge {
                    from,
                    to,
                    overlaps,
                    bounded,
                    hypothetical: !rules[from].executable() || !rules[to].executable(),
                }),
                Searched::Undecided(why) => undecided.push(Undecided { from, to, why }),
            }
        }
    }

    for scope in &mut scopes {
        scope.partial = scope
            .members
            .iter()
            .copied()
            .filter(|&rule| !rules[rule].partial.is_empty())
            .collect();
        scope.refused = scope
            .members
            .iter()
            .copied()
            .filter(|&rule| !rules[rule].executable())
            .collect();
        scope.undecided = undecided
            .iter()
            .filter(|pair| scope.holds(pair.from, pair.to))
            .count();
        let executable: Vec<usize> = scope
            .members
            .iter()
            .copied()
            .filter(|rule| !scope.refused.contains(rule))
            .collect();
        let supported: Vec<&Edge> = edges
            .iter()
            .filter(|edge| !edge.hypothetical && scope.holds(edge.from, edge.to))
            .collect();
        scope.cycles = cycles(&executable, &supported, rules, scope.complete());
    }
    Web {
        analysed,
        edges,
        undecided,
        scopes,
    }
}
