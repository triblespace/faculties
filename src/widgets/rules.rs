//! Avalanche rules from the selected workspace's frozen Rules evidence.
//!
//! Read-only: opening, refreshing or navigating this card never seeds, ticks,
//! selects, publishes or spawns anything. It shows what the Rules collection
//! states — revisions, readings, conclusion-shaped facts and the inputs they
//! name — and a static rule web. Evaluator status, firing history and causal
//! links are not stored, so they are not shown.

pub mod projection;
pub mod web;

use crate::widgets::{DatasetRevision, SourceKey, WidgetContext};
use egui::{RichText, Ui};
use projection::{
    short, ClauseKind, ConclusionLine, Described, Operation, ReadingState, RuleLine, RulesView,
};
use std::time::Duration;
use triblespace::prelude::Id;
use web::{Edge, Web};
use GORBIE::dataflow::ComputedState;
use GORBIE::prelude::CardCtx;

/// Rules supplies the facts; Schemas only names ids, from the same workspace.
pub(crate) const INPUTS: [SourceKey; 2] = [SourceKey::Rules, SourceKey::Schemas];

pub const SECTION: &str = "Avalanche rules";

type Key = [(Option<DatasetRevision>, bool); 2];

struct Loaded {
    view: RulesView,
    web: Web,
}

#[derive(Default)]
pub struct RulesViewer {
    result: ComputedState<Option<(Key, Result<std::sync::Arc<Loaded>, String>)>>,
    /// The opened revision; navigation state only, reset with the workspace.
    selected: Option<Id>,
    /// What a click asked for during the last frame, applied after drawing.
    clicked: Option<Option<Id>>,
}

impl RulesViewer {
    pub fn render(&mut self, ctx: &mut CardCtx<'_>, sources: WidgetContext<'_>) {
        ctx.section(SECTION, |ctx| {
            let key: Key = INPUTS.map(|key| (sources.revision(key), sources.missing_route(key)));
            self.result.poll();
            let current = self
                .result
                .value()
                .as_ref()
                .filter(|(cached, _)| *cached == key);
            let readable = sources.owned_dataset(SourceKey::Rules);
            if current.is_none() && !self.result.is_running() {
                if let Some(rules) = readable {
                    let schemas = sources.owned_dataset(SourceKey::Schemas);
                    self.result.spawn(ctx.ui_mut().ctx(), move || {
                        let loaded = rules.view().map_err(str::to_owned).map(|view| {
                            let schemas = schemas.as_ref().and_then(|s| s.view().ok());
                            let view = projection::project(view, schemas);
                            let web = web::web(&view.rules);
                            std::sync::Arc::new(Loaded { view, web })
                        });
                        Some((key, loaded))
                    });
                }
            }
            ctx.with_padding(12, |ctx| {
                let ui = ctx.ui_mut();
                if sources.missing_route(SourceKey::Rules) {
                    ui.label("Rules route missing from this workspace.");
                    ui.weak("This workspace does not configure a Rules collection, so it has no rules to show.");
                    return;
                }
                if !sources.contains(SourceKey::Rules) {
                    ui.label("Rules unavailable in this workspace.");
                    ui.weak("The Rules route is configured but its collection could not be read.");
                    return;
                }
                let state = match self.result.value() {
                    Some((cached, Ok(loaded))) if *cached == key => Ok(Some(loaded.clone())),
                    Some((cached, Err(error))) if *cached == key => Err(error.clone()),
                    _ => Ok(None),
                };
                match state {
                    Ok(Some(loaded)) => {
                        if sources.missing_route(SourceKey::Schemas)
                            || !sources.contains(SourceKey::Schemas)
                        {
                            ui.weak("Names unavailable: no readable Schemas in this workspace, so ids are shown.");
                        }
                        let opened = self
                            .selected
                            .and_then(|id| loaded.view.rules.iter().position(|r| r.revision == id));
                        match opened {
                            Some(index) => self.draw_detail(ui, &loaded, index),
                            None => self.draw_list(ui, &loaded),
                        }
                    }
                    Err(error) => {
                        ui.label(format!("Rules evidence unavailable: {error}"));
                    }
                    Ok(None) => {
                        ui.label("Reading rules…");
                        ui.ctx().request_repaint_after(Duration::from_millis(250));
                    }
                }
            });
            if let Some(next) = self.clicked.take() {
                self.selected = next;
            }
        });
    }

    fn open_button(&mut self, ui: &mut Ui, rule: &RuleLine) {
        if ui
            .selectable_label(false, RichText::new(rule.title()).strong())
            .on_hover_text(format!("{:x}", rule.revision))
            .clicked()
        {
            self.clicked = Some(Some(rule.revision));
        }
    }

    fn draw_list(&mut self, ui: &mut Ui, loaded: &Loaded) {
        let view = &loaded.view;
        ui.weak("Read-only: viewing evaluates, seeds and publishes nothing.");
        draw_environments(ui, view);
        ui.separator();
        if view.rules.is_empty() {
            ui.label("No rule revisions in this workspace's Rules collection.");
        } else {
            ui.label(
                RichText::new(format!(
                    "{} rule revision{}",
                    view.revisions_total,
                    if view.revisions_total == 1 { "" } else { "s" }
                ))
                .strong(),
            );
            if view.revisions_total > view.rules.len() {
                ui.weak(format!(
                    "Showing the first {} of {} revisions.",
                    view.rules.len(),
                    view.revisions_total
                ));
            }
            for rule in &view.rules {
                ui.horizontal_wrapped(|ui| {
                    self.open_button(ui, rule);
                    ui.weak(status(rule));
                });
                ui.indent(rule.revision, |ui| {
                    ui.small(format!(
                        "{} matches · {} core · {} emits{}",
                        rule.clauses(ClauseKind::Matches).count(),
                        rule.clauses(ClauseKind::Core).count(),
                        rule.clauses(ClauseKind::Emits).count(),
                        match rule.outputs.iter().map(|o| o.total).sum::<usize>() {
                            0 => String::new(),
                            n => format!(
                                " · {n} stored conclusion-shaped entit{}",
                                if n == 1 { "y" } else { "ies" }
                            ),
                        }
                    ));
                    if !rule.problems.is_empty() {
                        ui.weak(format!(
                            "{} problem(s) in this revision",
                            rule.problems.len()
                        ));
                    }
                });
            }
        }
        ui.weak(format!(
            "{} other entit{} in this collection (observations and conclusions; the pile does not tell them apart).",
            view.other_entities,
            if view.other_entities == 1 { "y" } else { "ies" }
        ));
        ui.separator();
        self.draw_web(ui, loaded);
    }

    fn draw_web(&mut self, ui: &mut Ui, loaded: &Loaded) {
        let (view, web) = (&loaded.view, &loaded.web);
        if view.rules.is_empty() {
            return;
        }
        ui.label(RichText::new("Rule web").strong());
        ui.weak(if web.selected_only {
            "Static analysis of the selected revisions: an edge means one rule can conclude a fact another rule matches, not that it did."
        } else {
            "No revision is selected, so this is a static what-if over all listed revisions: an edge means one rule can conclude a fact another matches."
        });
        if web.truncated {
            ui.weak(format!(
                "Only the first {} revisions are analysed.",
                web::MAX_WEB_RULES
            ));
        }
        if web.edges.is_empty() {
            ui.small("No rule can feed another (or itself).");
        }
        for edge in &web.edges {
            ui.small(edge_line(view, edge));
        }
        for cycle in &web.cycles {
            let names: Vec<String> = cycle.rules.iter().map(|&r| view.rules[r].title()).collect();
            ui.label(format!("Static feedback cycle: {}", names.join(" → ")));
            ui.weak(cycle_note(cycle.constructs));
        }
        ui.weak("Firings are not recorded in the pile, so observed firings are not shown; no cycle is claimed productive.");
    }

    fn draw_detail(&mut self, ui: &mut Ui, loaded: &Loaded, index: usize) {
        let view = &loaded.view;
        let rule = &view.rules[index];
        if ui.button("← All rules").clicked() {
            self.clicked = Some(None);
        }
        ui.heading(rule.title());
        ui.monospace(format!("revision {:x}", rule.revision));
        ui.label(status(rule));
        ui.small(match &rule.operation {
            Operation::Positive => "operation POSITIVE".to_owned(),
            Operation::Missing => "operation not stated".to_owned(),
            Operation::Unknown(ids) => format!(
                "operation {} (not one this viewer knows)",
                ids.iter()
                    .map(|id| short(*id))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        });
        for older in &rule.supersedes {
            ui.small(format!("supersedes {}", short(*older)));
        }
        for newer in &rule.superseded_by {
            ui.small(format!("superseded by {}", short(*newer)));
        }
        for problem in &rule.problems {
            ui.colored_label(ui.visuals().warn_fg_color, problem);
        }

        ui.separator();
        for (kind, heading) in [
            (ClauseKind::Matches, "Matches"),
            (ClauseKind::Core, "Concludes (identity core)"),
            (ClauseKind::Emits, "Concludes (emits)"),
        ] {
            let lines: Vec<String> = rule
                .clauses(kind)
                .map(|(_, clause)| rule.clause_text(clause))
                .collect();
            if lines.is_empty() {
                continue;
            }
            ui.label(RichText::new(heading).strong());
            for line in lines {
                ui.monospace(format!("  {line}"));
            }
        }
        for constraint in &rule.constraints {
            ui.monospace(format!(
                "  STRING_CONTAINS({}, {})",
                rule.terms[constraint.container].display(),
                rule.terms[constraint.needle].display()
            ));
        }
        egui::CollapsingHeader::new("Exact text projection")
            .id_salt(("rules-text", rule.revision))
            .show(ui, |ui| {
                ui.weak("triblespace's own text projection of this revision's stored facts; its aliases are generated.");
                ui.monospace(&rule.text);
            });

        ui.separator();
        ui.label(RichText::new("Stored conclusion-shaped facts").strong());
        ui.weak(
            "Entities in this collection that carry every core attribute of one of this rule's outputs. \
             The pile does not record which rule, reading or firing wrote them, or when; another rule or a \
             hand-written fact can have the same shape. A conclusion is a stored claim or request, not evidence \
             that any action happened.",
        );
        if rule.outputs.is_empty() {
            ui.small("This rule constructs no identity, so there is no output shape to look for.");
        }
        for output in &rule.outputs {
            ui.small(format!(
                "{}: {}{} stored entit{}",
                rule.terms[output.term].label,
                output.total,
                if output.capped { "+" } else { "" },
                if output.total == 1 { "y" } else { "ies" }
            ));
            if output.total == 0 {
                ui.weak("None stored yet.");
            }
            for conclusion in &output.conclusions {
                draw_conclusion(ui, rule, conclusion);
            }
            if output.total > output.conclusions.len() {
                ui.weak(format!("Showing {} of them.", output.conclusions.len()));
            }
        }
        for (clause, count) in &rule.annotations {
            ui.small(format!(
                "{}: {count} stored fact(s) with this attribute, from any source",
                rule.clause_text(&rule.clauses[*clause])
            ));
        }

        ui.separator();
        ui.label(RichText::new("Rule web").strong());
        let web = &loaded.web;
        if !web.considered.contains(&index) {
            ui.weak(
                "This revision is outside the analysed web (it is not selected while others are).",
            );
        } else {
            let feeds: Vec<&Edge> = web.feeds(index).collect();
            let fed: Vec<&Edge> = web.fed_by(index).collect();
            if feeds.is_empty() && fed.is_empty() {
                ui.small("It feeds no analysed rule, and none feeds it.");
            }
            for edge in feeds.iter().chain(fed.iter()) {
                draw_edge(ui, view, edge);
            }
            for cycle in web.cycles_through(index) {
                let names: Vec<String> =
                    cycle.rules.iter().map(|&r| view.rules[r].title()).collect();
                ui.label(format!("Static feedback cycle: {}", names.join(" → ")));
                ui.weak(cycle_note(cycle.constructs));
            }
        }
    }
}

fn status(rule: &RuleLine) -> String {
    let mut parts = Vec::new();
    if !rule.selected_in.is_empty() {
        parts.push(format!(
            "selected by the current reading of {}",
            rule.selected_in
                .iter()
                .map(|id| short(*id))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    } else if rule.named_by_readings > 0 {
        parts.push("named only by readings that are not current".to_owned());
    } else {
        parts.push("published, not selected".to_owned());
    }
    if !rule.superseded_by.is_empty() {
        parts.push("superseded".to_owned());
    }
    parts.join(" · ")
}

fn draw_environments(ui: &mut Ui, view: &RulesView) {
    if view.environments.is_empty() {
        ui.label("No reading selects any rule in this workspace.");
        return;
    }
    for environment in &view.environments {
        let anchor = short(environment.anchor);
        match &environment.state {
            ReadingState::Sole { reading, selects } => ui.label(format!(
                "Environment {anchor}: current reading {} selects {} revision{} ({} reading{} stored)",
                short(*reading),
                selects.len(),
                if selects.len() == 1 { "" } else { "s" },
                environment.readings,
                if environment.readings == 1 { "" } else { "s" },
            )),
            ReadingState::Empty => ui.label(format!(
                "Environment {anchor}: no current reading ({} superseded)",
                environment.readings
            )),
            ReadingState::Fork(readings) => ui.label(format!(
                "Environment {anchor}: readings fork ({}); it does not advance until one supersedes the others",
                readings.iter().map(|id| short(*id)).collect::<Vec<_>>().join(", ")
            )),
        };
    }
    ui.weak("A reading states what an environment selects. Whether an evaluator runs it is not recorded in the pile.");
}

fn draw_described(ui: &mut Ui, described: &Described) {
    for (attribute, value) in &described.facts {
        ui.monospace(format!("    {attribute}: {}", value.text));
    }
    if described.more > 0 {
        ui.weak(format!("    … {} more fact(s)", described.more));
    }
}

fn draw_conclusion(ui: &mut Ui, rule: &RuleLine, conclusion: &ConclusionLine) {
    ui.monospace(format!("  entity {}", short(conclusion.described.entity)));
    draw_described(ui, &conclusion.described);
    for input in &conclusion.inputs {
        let clause = &rule.clauses[input.clause];
        ui.small(format!(
            "    linked input {} = {} (named by its @{} fact)",
            rule.terms[input.term].label, input.value.text, clause.predicate_label
        ));
        match &input.described {
            Some(described) => draw_described(ui, described),
            None => {
                ui.weak("    not described in this Rules collection");
            }
        }
    }
}

fn edge_line(view: &RulesView, edge: &Edge) -> String {
    let (from, to) = (view.rules[edge.from].title(), view.rules[edge.to].title());
    let arrow = if edge.from == edge.to {
        format!("{from} can feed itself")
    } else {
        format!("{from} can feed {to}")
    };
    format!(
        "{arrow}: {} maximal overlap{}{}",
        edge.overlaps.len(),
        if edge.overlaps.len() == 1 { "" } else { "s" },
        if edge.bounded {
            " (search bounded)"
        } else {
            ""
        }
    )
}

fn draw_edge(ui: &mut Ui, view: &RulesView, edge: &Edge) {
    let (producer, consumer) = (&view.rules[edge.from], &view.rules[edge.to]);
    ui.label(edge_line(view, edge));
    for (n, overlap) in edge.overlaps.iter().enumerate() {
        ui.small(format!("  overlap {}", n + 1));
        for (matched, output) in &overlap.pairs {
            ui.monospace(format!(
                "    {} {} ← {} {}",
                consumer.title(),
                consumer.clause_text(&consumer.clauses[*matched]),
                producer.title(),
                producer.clause_text(&producer.clauses[*output]),
            ));
        }
        for binding in &overlap.bindings {
            let left: Vec<&str> = binding
                .producer
                .iter()
                .map(|&t| producer.terms[t].label.as_str())
                .collect();
            let right: Vec<&str> = binding
                .consumer
                .iter()
                .map(|&t| consumer.terms[t].label.as_str())
                .collect();
            let literals = if binding.literals.is_empty() {
                String::new()
            } else {
                format!(
                    " ∈ {{{}}}",
                    binding
                        .literals
                        .iter()
                        .map(|v| v.text.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            ui.monospace(format!(
                "    binds {} = {}{literals}",
                left.join(" = "),
                right.join(" = ")
            ));
        }
        if overlap.remaining.is_empty() {
            ui.small("    no remaining conditions");
        }
        for matched in &overlap.remaining {
            ui.monospace(format!(
                "    still needs {}",
                consumer.clause_text(&consumer.clauses[*matched])
            ));
        }
    }
}

fn cycle_note(constructs: bool) -> &'static str {
    if constructs {
        "A rule in this cycle constructs new identities: it may settle or keep growing. This viewer does not decide termination."
    } else {
        "No rule in this cycle constructs a new identity, so it can only relate values already present: it settles whenever its inputs are finite."
    }
}
