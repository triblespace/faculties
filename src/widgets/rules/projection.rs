//! A read-only projection of one frozen Rules observation.
//!
//! Every line here is a query over the selected workspace's Rules facts (and,
//! for labels only, its Schemas facts). Nothing evaluates, seeds, publishes or
//! selects a rule. The projection is rebuilt per observation revision and kept
//! only as presentation; it is not a catalogue other code should read.
//!
//! What the stored facts can and cannot say:
//! - a rule revision is an entity with `operation`/`matches`/`core`/`emits`
//!   links; its clauses and literal terms are ordinary facts;
//! - a reading (`reading_of`, `selects`) states which revisions an environment
//!   executes; competing readings resolve through `metadata::supersedes`
//!   exactly as the collection environment resolves them;
//! - a conclusion is an ordinary fact. The pile records neither which rule,
//!   reading or firing produced it, nor when an evaluator ran. A stored entity
//!   with a rule's conclusion shape is therefore shown as *shaped like* that
//!   rule's output, never as caused by it.

use std::collections::{BTreeMap, BTreeSet};

use triblespace::core::inline::encodings::genid::GenId;
use triblespace::core::inline::encodings::UnknownInline;
use triblespace::core::inline::{Inline, IntoInline, RawInline, TryFromInline};
use triblespace::core::metadata;
use triblespace::core::query::equalityconstraint::EqualityConstraint;
use triblespace::core::query::intersectionconstraint::IntersectionConstraint;
use triblespace::core::query::register::{sole, ObservationOrder, Resolution};
use triblespace::core::query::unionconstraint::UnionConstraint;
use triblespace::core::query::{Constraint, Query, TriblePattern, Variable};
use triblespace::core::reactive::collection::schema as environment;
use triblespace::core::reactive::schema as rule;
use triblespace::core::reactive::text;
use triblespace::core::trible::{Trible, TribleSet};
use triblespace::prelude::*;

use crate::storage::FactArchive;
use crate::widgets::DatasetView;

/// Rule revisions listed; more are counted, not decoded.
pub const MAX_RULES: usize = 256;
/// Conclusion-shaped entities listed per output term.
pub const MAX_CONCLUSIONS: usize = 64;
/// Conclusion-shaped entities counted per output term before "or more".
pub const COUNT_CAP: usize = 10_000;
/// Facts shown per described entity (a conclusion or a linked input).
pub const MAX_ENTITY_FACTS: usize = 32;
/// Linked inputs shown per conclusion.
pub const MAX_INPUTS: usize = 8;
/// Clauses decoded per rule part; a larger rule is shown as truncated.
pub const MAX_CLAUSES: usize = 64;

/// One decoded value, formatted without guessing an encoding the attribute
/// does not declare: id-shaped values print as ids, `created_at` as a time,
/// everything else as its raw bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Value {
    pub raw: RawInline,
    pub text: String,
    pub id: Option<Id>,
}

/// A revision-local term: a variable, or a constant with literal alternatives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Term {
    pub id: Id,
    /// `?a`, `?b`, … in order of first appearance within this revision.
    pub label: String,
    pub literals: Vec<Value>,
}

impl Term {
    pub fn display(&self) -> String {
        match self.literals.as_slice() {
            [] => self.label.clone(),
            [one] => one.text.clone(),
            many => format!(
                "{} ∈ {{{}}}",
                self.label,
                many.iter()
                    .map(|value| value.text.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClauseKind {
    Matches,
    Core,
    Emits,
}

impl ClauseKind {
    pub fn keyword(self) -> &'static str {
        match self {
            Self::Matches => "matches",
            Self::Core => "core",
            Self::Emits => "emits",
        }
    }
}

/// One `(subject, predicate, object)` clause with indices into `RuleLine::terms`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Clause {
    pub id: Id,
    pub kind: ClauseKind,
    pub subject: usize,
    pub predicate: Id,
    pub predicate_label: String,
    pub object: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContentConstraint {
    pub id: Id,
    pub container: usize,
    pub needle: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Operation {
    Positive,
    Missing,
    Unknown(Vec<Id>),
}

/// How one environment's readings resolve, as the collection environment
/// resolves them. Execution status is not stored, so none is claimed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadingState {
    /// No reading names this environment.
    Empty,
    /// Exactly one current reading.
    Sole { reading: Id, selects: Vec<Id> },
    /// Several current readings; the environment would refuse to advance.
    Fork(Vec<Id>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvironmentLine {
    pub anchor: Id,
    pub readings: usize,
    pub state: ReadingState,
}

/// One entity described by its facts in this collection (bounded).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Described {
    pub entity: Id,
    pub facts: Vec<(String, Value)>,
    pub more: usize,
}

/// A value of a conclusion that names an entity bound by the rule's matches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkedInput {
    /// The clause of this rule through which the conclusion names the input.
    pub clause: usize,
    pub term: usize,
    /// The input's facts in this Rules collection, when it has any.
    pub described: Option<Described>,
    pub value: Value,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConclusionLine {
    pub described: Described,
    pub inputs: Vec<LinkedInput>,
}

/// Stored entities carrying every core attribute of one output term.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputShape {
    pub term: usize,
    pub core: Vec<usize>,
    pub conclusions: Vec<ConclusionLine>,
    /// Counted matches, capped at [`COUNT_CAP`].
    pub total: usize,
    pub capped: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuleLine {
    pub revision: Id,
    pub names: Vec<String>,
    pub operation: Operation,
    pub terms: Vec<Term>,
    pub clauses: Vec<Clause>,
    pub constraints: Vec<ContentConstraint>,
    /// Revisions this one says it supersedes, and those superseding it.
    pub supersedes: Vec<Id>,
    pub superseded_by: Vec<Id>,
    /// Environments whose current reading selects this revision.
    pub selected_in: Vec<Id>,
    /// Readings (current or not) that name this revision.
    pub named_by_readings: usize,
    /// Why the engine would refuse this revision if it were selected (or error
    /// at its first firing), mirroring `reactive::Batch::new` and `fire`. A
    /// refused revision concludes nothing, so its edges are structural only.
    pub refused: Vec<String>,
    /// Parts this viewer did not decode (bounds). The engine may read them;
    /// what is shown of this revision is then incomplete, not wrong.
    pub partial: Vec<String>,
    /// Valid shapes worth saying, e.g. a multi-valued clause read as every
    /// combination, exactly as the engine reads it.
    pub notes: Vec<String>,
    /// `triblespace::core::reactive::text::write` of this revision's own facts.
    pub text: String,
    pub outputs: Vec<OutputShape>,
    /// Emits clauses annotating a matched (existing) subject: only the number
    /// of stored facts with that attribute is known.
    pub annotations: Vec<(usize, usize)>,
}

impl RuleLine {
    pub fn title(&self) -> String {
        if self.names.is_empty() {
            format!("rule {}", short(self.revision))
        } else {
            self.names.join(" / ")
        }
    }

    pub fn clauses(&self, kind: ClauseKind) -> impl Iterator<Item = (usize, &Clause)> {
        self.clauses
            .iter()
            .enumerate()
            .filter(move |(_, clause)| clause.kind == kind)
    }

    pub fn clause_text(&self, clause: &Clause) -> String {
        format!(
            "({} @{} {})",
            self.terms[clause.subject].display(),
            clause.predicate_label,
            self.terms[clause.object].display()
        )
    }

    /// Terms bound by this revision's matches.
    pub fn matched_terms(&self) -> BTreeSet<usize> {
        self.clauses(ClauseKind::Matches)
            .flat_map(|(_, clause)| [clause.subject, clause.object])
            .collect()
    }

    /// Subjects of core clauses: the identities this rule constructs.
    pub fn constructed_terms(&self) -> BTreeSet<usize> {
        self.clauses(ClauseKind::Core)
            .map(|(_, clause)| clause.subject)
            .collect()
    }

    /// Terms with a value before any construction: matched, or literal.
    pub fn bound_terms(&self) -> BTreeSet<usize> {
        let mut bound = self.matched_terms();
        bound.extend(
            self.terms
                .iter()
                .enumerate()
                .filter(|(_, term)| !term.literals.is_empty())
                .map(|(index, _)| index),
        );
        bound
    }

    /// The engine would run this revision when selected, as far as its stored
    /// facts show; content and firing-time data are not judged here.
    pub fn executable(&self) -> bool {
        self.refused.is_empty()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RulesView {
    pub environments: Vec<EnvironmentLine>,
    /// Revisions a current reading selects come first, so a selection is
    /// never lost to the listing bound before the unselected ones are.
    pub rules: Vec<RuleLine>,
    /// All revisions found, including those beyond [`MAX_RULES`].
    pub revisions_total: usize,
    /// Revisions a current reading selects that have no rule facts in this
    /// collection: their bodies are not here, so they cannot be analysed.
    pub selected_missing: Vec<Id>,
    /// Selected revisions present but beyond [`MAX_RULES`].
    pub selected_unlisted: usize,
    /// Entities that are neither program nor reading: observations and
    /// conclusions alike, which the pile does not tell apart.
    pub other_entities: usize,
    pub schemas_unavailable: bool,
}

pub fn short(id: Id) -> String {
    format!("{id:x}").chars().take(8).collect()
}

fn raw_id(raw: &RawInline) -> Option<Id> {
    Id::try_from_inline(&Inline::<GenId>::new(*raw)).ok()
}

/// Format a value without inventing an encoding: ids, `created_at`, or bytes.
pub fn value(attribute: Option<Id>, raw: RawInline) -> Value {
    let id = raw_id(&raw);
    let text = if attribute == Some(metadata::created_at.id()) {
        crate::care::format_time(Inline::<inlineencodings::NsTAIInterval>::new(raw))
    } else if let Some(id) = id {
        short(id)
    } else {
        let hex = hex::encode(raw);
        format!("0x{}…", &hex[..16])
    };
    Value { raw, text, id }
}

/// Every value of `attribute` on `entity`, as raw inline bytes.
fn values(facts: &FactArchive, entity: Id, attribute: Id) -> Vec<RawInline> {
    find!(
        v: Inline<UnknownInline>,
        facts.pattern::<UnknownInline>(entity.to_inline(), attribute.to_inline(), v)
    )
    .map(|v| v.raw)
    .collect()
}

/// The ids among `attribute`'s values on `entity`.
fn ids(facts: &FactArchive, entity: Id, attribute: Id) -> Vec<Id> {
    values(facts, entity, attribute)
        .iter()
        .filter_map(raw_id)
        .collect()
}

/// A single id-valued field, or why there is not exactly one.
fn single(facts: &FactArchive, entity: Id, attribute: Id) -> Result<Id, &'static str> {
    match ids(facts, entity, attribute).as_slice() {
        [one] => Ok(*one),
        [] => Err("missing"),
        _ => Err("ambiguous"),
    }
}

/// Facts about one entity, ordered, bounded.
pub fn describe(
    facts: &FactArchive,
    entity: Id,
    label: &dyn Fn(Id) -> String,
) -> Option<Described> {
    let mut rows: Vec<(Id, RawInline)> = find!(
        (a: Id, v: Inline<UnknownInline>),
        facts.pattern::<UnknownInline>(entity.to_inline(), a, v)
    )
    .map(|(a, v)| (a, v.raw))
    .take(MAX_ENTITY_FACTS + 1_000)
    .collect();
    if rows.is_empty() {
        return None;
    }
    rows.sort();
    let more = rows.len().saturating_sub(MAX_ENTITY_FACTS);
    rows.truncate(MAX_ENTITY_FACTS);
    Some(Described {
        entity,
        facts: rows
            .into_iter()
            .map(|(a, raw)| (label(a), value(Some(a), raw)))
            .collect(),
        more,
    })
}

/// Facts with `entity` in subject position, as tribles (for the exact text).
fn subgraph(facts: &FactArchive, entity: Id, into: &mut TribleSet) {
    for (a, v) in find!(
        (a: Id, v: Inline<UnknownInline>),
        facts.pattern::<UnknownInline>(entity.to_inline(), a, v)
    ) {
        into.insert(&Trible::force(&entity, &a, &v));
    }
}

/// Names for ids, from the same workspace's Schemas collection only.
struct Names<'a> {
    schemas: Option<DatasetView<'a>>,
    known: std::cell::RefCell<BTreeMap<Id, Vec<String>>>,
}

impl<'a> Names<'a> {
    fn names(&self, id: Id) -> Vec<String> {
        if let Some(names) = self.known.borrow().get(&id) {
            return names.clone();
        }
        let names = self
            .schemas
            .and_then(|schemas| {
                crate::schema_catalog::named_entry(schemas.reader, schemas.facts, id)
                    .ok()
                    .flatten()
            })
            .map(|entry| entry.names)
            .unwrap_or_default();
        self.known.borrow_mut().insert(id, names.clone());
        names
    }

    fn label(&self, id: Id) -> String {
        let names = self.names(id);
        if names.is_empty() {
            short(id)
        } else {
            names.join(" / ")
        }
    }
}

fn environments(facts: &FactArchive) -> Vec<EnvironmentLine> {
    let mut by_anchor = BTreeMap::<Id, BTreeSet<Id>>::new();
    for (reading, anchor) in find!(
        (reading: Id, anchor: Id),
        pattern!(facts, [{ ?reading @ environment::reading_of: ?anchor }])
    ) {
        by_anchor.entry(anchor).or_default().insert(reading);
    }
    let order = ObservationOrder::new(facts, metadata::supersedes.id());
    by_anchor
        .into_iter()
        .map(|(anchor, readings)| {
            let count = readings.len();
            let state = match sole(&order, readings) {
                Resolution::Empty => ReadingState::Empty,
                Resolution::Sole(reading) => {
                    let mut selects = ids(facts, reading, environment::selects.id());
                    selects.sort();
                    selects.dedup();
                    ReadingState::Sole { reading, selects }
                }
                Resolution::Fork(set) => ReadingState::Fork(set.into_iter().collect()),
            };
            EnvironmentLine {
                anchor,
                readings: count,
                state,
            }
        })
        .collect()
}

/// Entities that look like rule revisions: anything with an operation or a
/// matches/core/emits link. A malformed one is listed with its problems.
fn revisions(facts: &FactArchive) -> BTreeSet<Id> {
    let mut found = BTreeSet::new();
    found.extend(find!(r: Id, pattern!(facts, [{ ?r @ rule::operation: _?o }])));
    found.extend(find!(r: Id, pattern!(facts, [{ ?r @ rule::matches: _?c }])));
    found.extend(find!(r: Id, pattern!(facts, [{ ?r @ rule::core: _?c }])));
    found.extend(find!(r: Id, pattern!(facts, [{ ?r @ rule::emits: _?c }])));
    // A content constraint entity carries `operation` too; it is not a rule.
    let constraints: BTreeSet<Id> = find!(
        c: Id,
        pattern!(facts, [{ _?r @ rule::constraints: ?c }])
    )
    .collect();
    found.retain(|id| !constraints.contains(id));
    found
}

struct Decoder<'f> {
    facts: &'f FactArchive,
    terms: Vec<Term>,
    index: BTreeMap<Id, usize>,
}

impl Decoder<'_> {
    fn term(&mut self, id: Id) -> usize {
        if let Some(index) = self.index.get(&id) {
            return *index;
        }
        let index = self.terms.len();
        let mut literals: Vec<RawInline> = values(self.facts, id, rule::literal.id());
        literals.sort();
        self.terms.push(Term {
            id,
            label: term_label(index),
            literals: literals.into_iter().map(|raw| value(None, raw)).collect(),
        });
        self.index.insert(id, index);
        index
    }
}

fn term_label(index: usize) -> String {
    let letter = (b'a' + (index % 26) as u8) as char;
    if index < 26 {
        format!("?{letter}")
    } else {
        format!("?{letter}{}", index / 26)
    }
}

/// Every distinct id among `attribute`'s values, sorted.
fn sorted_ids(facts: &FactArchive, entity: Id, attribute: Id) -> Vec<Id> {
    let mut found = ids(facts, entity, attribute);
    found.sort();
    found.dedup();
    found
}

/// Names of the empty fields, for a refusal cause.
fn missing_fields(fields: [(&[Id], &str); 3]) -> String {
    fields
        .iter()
        .filter(|(values, _)| values.is_empty())
        .map(|(_, name)| *name)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Combinations decoded from one multi-valued clause or constraint.
pub const MAX_COMBINATIONS: usize = 16;

fn decode(facts: &FactArchive, revision: Id, names: &Names<'_>) -> RuleLine {
    let mut refused = Vec::new();
    let mut partial = Vec::new();
    let mut notes = Vec::new();
    let operations = sorted_ids(facts, revision, rule::operation.id());
    let operation = match operations.as_slice() {
        [] => Operation::Missing,
        [one] if *one == rule::POSITIVE => Operation::Positive,
        other => Operation::Unknown(other.to_vec()),
    };
    // As `Batch::new`: no operation, or any operation other than POSITIVE
    // (even beside POSITIVE), is a selected-program error.
    match &operation {
        Operation::Positive => {}
        Operation::Missing => {
            refused.push("no operation is stated: the engine refuses it when selected".to_owned())
        }
        Operation::Unknown(ids) => refused.push(format!(
            "operation {} is not POSITIVE alone: the engine refuses it when selected",
            ids.iter()
                .map(|id| short(*id))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
    let mut decoder = Decoder {
        facts,
        terms: Vec::new(),
        index: BTreeMap::new(),
    };
    let mut clauses = Vec::new();
    let mut program = TribleSet::new();
    subgraph(facts, revision, &mut program);
    for (kind, attribute) in [
        (ClauseKind::Matches, rule::matches.id()),
        (ClauseKind::Core, rule::core.id()),
        (ClauseKind::Emits, rule::emits.id()),
    ] {
        let linked = sorted_ids(facts, revision, attribute);
        let mut decoded = 0;
        for clause in linked {
            subgraph(facts, clause, &mut program);
            let [subjects, predicates, objects] =
                [rule::subject.id(), rule::predicate.id(), rule::object.id()]
                    .map(|field| sorted_ids(facts, clause, field));
            if subjects.is_empty() || predicates.is_empty() || objects.is_empty() {
                // `Batch::new`: a linked clause with no complete tuple.
                refused.push(format!(
                    "{} clause {} has no {}: the engine refuses it when selected",
                    kind.keyword(),
                    short(clause),
                    missing_fields([
                        (subjects.as_slice(), "subject"),
                        (predicates.as_slice(), "predicate"),
                        (objects.as_slice(), "object"),
                    ])
                ));
                continue;
            }
            // The engine reads every (subject, predicate, object) combination
            // of a multi-valued clause as its own clause; so does this viewer.
            let combinations = subjects.len() * predicates.len() * objects.len();
            if combinations > 1 {
                notes.push(format!(
                    "{} clause {} has {combinations} value combinations, each read as a clause as the engine does",
                    kind.keyword(),
                    short(clause)
                ));
            }
            let mut taken = 0;
            'combinations: for &s in &subjects {
                for &predicate in &predicates {
                    for &o in &objects {
                        if taken == MAX_COMBINATIONS || decoded == MAX_CLAUSES {
                            partial.push(format!(
                                "{} clauses beyond the first {MAX_CLAUSES} (or a clause's combinations beyond {MAX_COMBINATIONS}) are not decoded here",
                                kind.keyword()
                            ));
                            break 'combinations;
                        }
                        taken += 1;
                        decoded += 1;
                        let subject = decoder.term(s);
                        let object = decoder.term(o);
                        clauses.push(Clause {
                            id: clause,
                            kind,
                            subject,
                            predicate,
                            predicate_label: names.label(predicate),
                            object,
                        });
                    }
                }
            }
        }
    }
    let mut constraints = Vec::new();
    let linked = sorted_ids(facts, revision, rule::constraints.id());
    for constraint in linked {
        subgraph(facts, constraint, &mut program);
        let [operations, containers, needles] = [
            rule::operation.id(),
            rule::container.id(),
            rule::needle.id(),
        ]
        .map(|field| sorted_ids(facts, constraint, field));
        if operations.is_empty() || containers.is_empty() || needles.is_empty() {
            refused.push(format!(
                "constraint {} has no {}: the engine refuses it when selected",
                short(constraint),
                missing_fields([
                    (operations.as_slice(), "operation"),
                    (containers.as_slice(), "container"),
                    (needles.as_slice(), "needle"),
                ])
            ));
            continue;
        }
        if let Some(op) = operations.iter().find(|op| **op != rule::STRING_CONTAINS) {
            refused.push(format!(
                "constraint {} names operation {}, which the engine does not support: it refuses it when selected",
                short(constraint),
                short(*op)
            ));
            continue;
        }
        let mut taken = 0;
        'pairs: for &container in &containers {
            for &needle in &needles {
                if taken == MAX_COMBINATIONS || constraints.len() == MAX_CLAUSES {
                    partial.push(format!(
                        "constraints beyond the first {MAX_CLAUSES} (or a constraint's combinations beyond {MAX_COMBINATIONS}) are not decoded here"
                    ));
                    break 'pairs;
                }
                taken += 1;
                constraints.push(ContentConstraint {
                    id: constraint,
                    container: decoder.term(container),
                    needle: decoder.term(needle),
                });
            }
        }
    }
    partial.dedup();
    // Literals take the encoding of an attribute they meet; reformat those
    // that meet `created_at`, the one attribute formatted specially.
    for clause in &clauses {
        if clause.predicate == metadata::created_at.id() {
            let term = &mut decoder.terms[clause.object];
            for literal in &mut term.literals {
                *literal = value(Some(clause.predicate), literal.raw);
            }
        }
    }
    for term in &decoder.terms {
        if !term.literals.is_empty() {
            subgraph_literals(facts, term.id, &mut program);
        }
    }
    let mut text = String::new();
    if text::write(&program, &mut text).is_err() {
        text = "The exact text projection could not be written.".to_owned();
    }
    let mut supersedes = ids(facts, revision, metadata::supersedes.id());
    supersedes.sort();
    let mut superseded_by: Vec<Id> = find!(
        newer: Id,
        pattern!(facts, [{ ?newer @ metadata::supersedes: revision }])
    )
    .collect();
    superseded_by.sort();
    let named_by_readings = find!(
        reading: Id,
        pattern!(facts, [{ ?reading @ environment::selects: revision }])
    )
    .count();
    let mut line = RuleLine {
        revision,
        names: names.names(revision),
        operation,
        terms: decoder.terms,
        clauses,
        constraints,
        supersedes,
        superseded_by,
        selected_in: Vec::new(),
        named_by_readings,
        refused,
        partial,
        notes,
        text,
        outputs: Vec::new(),
        annotations: Vec::new(),
    };
    // The remaining engine checks need the whole program; on a partly decoded
    // revision they could misjudge, so they are not made.
    if line.partial.is_empty() {
        let causes = engine_refusals(&line);
        line.refused.extend(causes);
    }
    line
}

/// The engine's remaining program errors, decided from the decoded program
/// exactly as `reactive::Batch::new` (selection) and `Batch::fire` (first
/// firing) decide them. Scratch sets live for this call only.
fn engine_refusals(line: &RuleLine) -> Vec<String> {
    let mut causes = Vec::new();
    let bound = line.bound_terms();
    let label = |term: usize| line.terms[term].label.clone();
    // One slot per term, plus an equality helper per same-term matches clause.
    let helpers = line
        .clauses(ClauseKind::Matches)
        .filter(|(_, clause)| clause.subject == clause.object)
        .count();
    if line.terms.len() + helpers > 128 {
        causes.push(
            "it needs more than 128 query variable slots: the engine refuses it when selected"
                .to_owned(),
        );
    }
    for constraint in &line.constraints {
        for term in [constraint.container, constraint.needle] {
            if !bound.contains(&term) {
                causes.push(format!(
                    "constraint {} reads {}, which no match or literal binds: the engine refuses it when selected",
                    short(constraint.id),
                    label(term)
                ));
            }
        }
    }
    let constructed = line.constructed_terms();
    for &term in &constructed {
        if bound.contains(&term) {
            causes.push(format!(
                "core subject {} is already bound by a match or literal: the engine errors at its first firing",
                label(term)
            ));
        }
    }
    // Resolve constructions in dependency order, as `fire` does.
    let mut values = bound.clone();
    loop {
        let ready: Vec<usize> = constructed
            .iter()
            .copied()
            .filter(|term| !values.contains(term))
            .filter(|&term| {
                line.clauses(ClauseKind::Core)
                    .filter(|(_, clause)| clause.subject == term)
                    .all(|(_, clause)| values.contains(&clause.object))
            })
            .collect();
        if ready.is_empty() {
            break;
        }
        values.extend(ready);
    }
    let unresolved: Vec<String> = (0..line.terms.len())
        .filter(|term| !values.contains(term))
        .map(label)
        .collect();
    if !unresolved.is_empty() {
        causes.push(format!(
            "{} {} no value (no match, literal or constructible core): the engine errors at its first firing",
            unresolved.join(", "),
            if unresolved.len() == 1 { "has" } else { "have" }
        ));
    }
    causes
}

fn subgraph_literals(facts: &FactArchive, term: Id, into: &mut TribleSet) {
    let attribute = rule::literal.id();
    for v in find!(
        v: Inline<UnknownInline>,
        facts.pattern::<UnknownInline>(term.to_inline(), attribute.to_inline(), v)
    ) {
        into.insert(&Trible::force(&term, &attribute, &v));
    }
}

/// Stored entities carrying every core attribute of `term`, with literal
/// objects fixed and shared object terms equal. This finds a shape, not a cause.
fn shaped<'f>(
    facts: &'f FactArchive,
    line: &RuleLine,
    term: usize,
    core: &[usize],
) -> impl Iterator<Item = Id> + 'f {
    let mut slots = BTreeMap::<usize, usize>::new();
    slots.insert(term, 0);
    let mut constraints: Vec<Box<dyn Constraint<'f> + 'f>> = Vec::new();
    let mut next = 1;
    for &index in core {
        let clause = &line.clauses[index];
        let predicate = clause.predicate.to_inline();
        let literals = &line.terms[clause.object].literals;
        if clause.object == term {
            // Same-term subject and value: native equality, as the engine does.
            let helper = next;
            next += 1;
            constraints.push(Box::new(facts.pattern(
                Variable::<GenId>::new(0),
                predicate,
                Variable::<UnknownInline>::new(helper),
            )));
            constraints.push(Box::new(EqualityConstraint::new(0, helper)));
        } else if !literals.is_empty() {
            let alternatives: Vec<Box<dyn Constraint<'f> + 'f>> = literals
                .iter()
                .map(|literal| {
                    Box::new(facts.pattern(
                        Variable::<GenId>::new(0),
                        predicate,
                        Inline::<UnknownInline>::new(literal.raw),
                    )) as Box<dyn Constraint<'f> + 'f>
                })
                .collect();
            constraints.push(Box::new(UnionConstraint::new(alternatives)));
        } else {
            let slot = *slots.entry(clause.object).or_insert_with(|| {
                next += 1;
                next - 1
            });
            constraints.push(Box::new(facts.pattern(
                Variable::<GenId>::new(0),
                predicate,
                Variable::<UnknownInline>::new(slot),
            )));
        }
    }
    let mut seen = BTreeSet::new();
    Query::new(
        IntersectionConstraint::new(constraints),
        |binding: &triblespace::core::query::Binding<'_>| binding.get(0).and_then(raw_id),
    )
    .filter(move |entity| seen.insert(*entity))
}

fn conclusions(facts: &FactArchive, line: &mut RuleLine, names: &Names<'_>) {
    let matched = line.matched_terms();
    let constructed = line.constructed_terms();
    let label = |id: Id| names.label(id);
    let mut outputs = Vec::new();
    for &term in &constructed {
        if matched.contains(&term) {
            // Constructing a matched term is a program error; nothing to find.
            continue;
        }
        let core: Vec<usize> = line
            .clauses(ClauseKind::Core)
            .filter(|(_, clause)| clause.subject == term)
            .map(|(index, _)| index)
            .collect();
        let mut total = 0;
        let mut listed = Vec::new();
        for entity in shaped(facts, line, term, &core).take(COUNT_CAP + 1) {
            total += 1;
            if listed.len() < MAX_CONCLUSIONS {
                listed.push(entity);
            }
        }
        let capped = total > COUNT_CAP;
        total = total.min(COUNT_CAP);
        listed.sort();
        let lines = listed
            .into_iter()
            .filter_map(|entity| {
                let described = describe(facts, entity, &label)?;
                let mut inputs = Vec::new();
                for (index, clause) in line.clauses.iter().enumerate() {
                    if clause.kind == ClauseKind::Matches
                        || clause.subject != term
                        || !matched.contains(&clause.object)
                        || inputs.len() >= MAX_INPUTS
                    {
                        continue;
                    }
                    for raw in values(facts, entity, clause.predicate) {
                        if inputs.len() >= MAX_INPUTS {
                            break;
                        }
                        let value = value(Some(clause.predicate), raw);
                        let described = value.id.and_then(|input| describe(facts, input, &label));
                        inputs.push(LinkedInput {
                            clause: index,
                            term: clause.object,
                            described,
                            value,
                        });
                    }
                }
                Some(ConclusionLine { described, inputs })
            })
            .collect();
        outputs.push(OutputShape {
            term,
            core,
            conclusions: lines,
            total,
            capped,
        });
    }
    let annotations = line
        .clauses(ClauseKind::Emits)
        .filter(|(_, clause)| !constructed.contains(&clause.subject))
        .map(|(index, clause)| {
            let predicate = clause.predicate.to_inline();
            let count = find!(
                (e: Id, v: Inline<UnknownInline>),
                facts.pattern::<UnknownInline>(e, predicate, v)
            )
            .take(COUNT_CAP)
            .count();
            (index, count)
        })
        .collect();
    line.outputs = outputs;
    line.annotations = annotations;
}

/// Build the whole projection for one frozen observation.
pub fn project(rules: DatasetView<'_>, schemas: Option<DatasetView<'_>>) -> RulesView {
    let facts = rules.facts;
    let names = Names {
        schemas,
        known: Default::default(),
    };
    let environments = environments(facts);
    let found = revisions(facts);
    let revisions_total = found.len();
    // What current readings select, chosen from the readings themselves
    // before the listing bound, so no selection hides behind unselected ids.
    let selected: BTreeSet<Id> = environments
        .iter()
        .filter_map(|environment| match &environment.state {
            ReadingState::Sole { selects, .. } => Some(selects.iter().copied()),
            _ => None,
        })
        .flatten()
        .collect();
    let selected_missing: Vec<Id> = selected
        .iter()
        .filter(|id| !found.contains(*id))
        .copied()
        .collect();
    let mut lines: Vec<RuleLine> = found
        .iter()
        .filter(|id| selected.contains(*id))
        .chain(found.iter().filter(|id| !selected.contains(*id)))
        .take(MAX_RULES)
        .map(|revision| decode(facts, *revision, &names))
        .collect();
    let selected_listed = lines
        .iter()
        .filter(|line| selected.contains(&line.revision))
        .count();
    let selected_unlisted = selected.len() - selected_missing.len() - selected_listed;
    for line in &mut lines {
        line.selected_in = environments
            .iter()
            .filter(|environment| match &environment.state {
                ReadingState::Sole { selects, .. } => selects.contains(&line.revision),
                _ => false,
            })
            .map(|environment| environment.anchor)
            .collect();
        conclusions(facts, line, &names);
    }
    // Selected first, then by title and id: a stable, explainable order.
    lines.sort_by(|a, b| {
        (a.selected_in.is_empty(), a.title(), a.revision).cmp(&(
            b.selected_in.is_empty(),
            b.title(),
            b.revision,
        ))
    });
    let mut program = found.clone();
    for line in &lines {
        program.extend(line.clauses.iter().map(|clause| clause.id));
        program.extend(line.constraints.iter().map(|constraint| constraint.id));
        program.extend(line.terms.iter().map(|term| term.id));
    }
    program.extend(find!(
        reading: Id,
        pattern!(facts, [{ ?reading @ environment::reading_of: _?anchor }])
    ));
    let mut entities = BTreeSet::new();
    for (e, _, _) in find!(
        (e: Id, a: Id, v: Inline<UnknownInline>),
        facts.pattern::<UnknownInline>(e, a, v)
    )
    .take(COUNT_CAP * 8)
    {
        if !program.contains(&e) {
            entities.insert(e);
        }
    }
    RulesView {
        environments,
        rules: lines,
        revisions_total,
        selected_missing,
        selected_unlisted,
        other_entities: entities.len(),
        schemas_unavailable: schemas.is_none(),
    }
}
