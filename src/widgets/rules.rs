//! Avalanche rules from the selected workspace's frozen Rules evidence.
//!
//! Read-only: opening, refreshing or navigating this card never seeds, ticks,
//! selects, publishes or spawns anything. It shows what the Rules collection
//! states — revisions, readings, conclusion-shaped facts and the inputs they
//! name — and a static rule web. Evaluator status, firing history and causal
//! links are not stored, so they are not shown.

pub mod diagram;
pub mod projection;
pub mod web;

use crate::widgets::{DatasetRevision, SourceKey, WidgetContext};
use egui::{RichText, Ui};
use projection::{
    short, ClauseKind, ConclusionLine, Described, Operation, ReadingState, RuleLine, RulesView,
};
use std::time::Duration;
use triblespace::prelude::Id;
use web::{Binding, Condition, Cycle, Edge, Scope, ScopeKind, Undecided, Unsearched, Web};
use GORBIE::dataflow::ComputedState;
use GORBIE::graph;
use GORBIE::prelude::CardCtx;

/// Edges listed at once (per scope, or per opened rule).
pub const MAX_LISTED_EDGES: usize = 48;
/// Undecided pairs listed at once.
pub const MAX_LISTED_UNDECIDED: usize = 16;
/// Rules drawn in the diagram; the list still names every listed edge.
pub const MAX_DIAGRAM_NODES: usize = 32;
/// Links drawn in the diagram.
pub const MAX_DIAGRAM_EDGES: usize = 256;

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
    /// The inspected edge, by producer and consumer revision.
    edge: Option<(Id, Id)>,
    /// The shown web scope, by position (clamped to what exists).
    scope: usize,
    /// What a click asked for during the last frame, applied after drawing.
    clicked: Option<Option<Id>>,
    clicked_edge: Option<Option<(Id, Id)>>,
    clicked_scope: Option<usize>,
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
                            let web = web::web(&view);
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
            if let Some(next) = self.clicked_edge.take() {
                self.edge = next;
            }
            if let Some(next) = self.clicked_scope.take() {
                self.scope = next;
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
                    "Showing the first {} of {} revisions, selected ones first.",
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
                    if let Some(cause) = rule.refused.first() {
                        ui.colored_label(
                            ui.visuals().warn_fg_color,
                            format!(
                                "refused by the engine: {cause}{}",
                                match rule.refused.len() {
                                    1 => String::new(),
                                    n => format!(" (+{} more)", n - 1),
                                }
                            ),
                        );
                    }
                    if !rule.partial.is_empty() {
                        ui.weak("only partly decoded here; open it for what is missing");
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

    /// The edge the reader inspects, if it is in this web.
    fn inspected<'w>(&self, view: &RulesView, web: &'w Web) -> Option<&'w Edge> {
        let (from, to) = self.edge?;
        let from = view.rules.iter().position(|rule| rule.revision == from)?;
        let to = view.rules.iter().position(|rule| rule.revision == to)?;
        web.edge(from, to)
    }

    fn edge_row(&mut self, ui: &mut Ui, view: &RulesView, edge: &Edge) {
        let key = (view.rules[edge.from].revision, view.rules[edge.to].revision);
        let chosen = self.edge == Some(key);
        if ui
            .selectable_label(chosen, RichText::new(edge_line(view, edge)).small())
            .on_hover_text("Show this edge's clauses, bindings and remaining conditions")
            .clicked()
        {
            self.clicked_edge = Some(if chosen { None } else { Some(key) });
        }
    }

    fn draw_web(&mut self, ui: &mut Ui, loaded: &Loaded) {
        let (view, web) = (&loaded.view, &loaded.web);
        if web.scopes.is_empty() {
            return;
        }
        ui.label(RichText::new("Rule web").strong());
        let current = self.scope.min(web.scopes.len() - 1);
        if web.scopes.len() > 1 {
            ui.weak(
                "Current readings select different revisions. Each scope is one reading's selection; \
                 revisions selected by different readings never run together, so they are never combined here.",
            );
            ui.horizontal_wrapped(|ui| {
                for (n, scope) in web.scopes.iter().enumerate() {
                    if ui
                        .selectable_label(n == current, RichText::new(scope_title(scope)))
                        .clicked()
                    {
                        self.clicked_scope = Some(n);
                    }
                }
            });
        }
        let scope = &web.scopes[current];
        ui.weak(scope_intro(scope));
        draw_gaps(ui, view, scope);
        for &rule in &scope.refused {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                format!(
                    "{} is refused by the engine ({}); its edges are structural only and it is in no feedback cycle.",
                    view.rules[rule].title(),
                    view.rules[rule].refused.join("; ")
                ),
            );
        }
        self.draw_diagram(ui, view, web, scope);

        let edges: Vec<&Edge> = web.edges_in(scope).collect();
        if edges.is_empty() {
            ui.small(if scope.complete() {
                "No analysed rule can feed another (or itself)."
            } else {
                "No edge was witnessed. The evidence above is incomplete, so this does not say that none exists."
            });
        } else {
            ui.small(format!(
                "{} witnessed edge{}; select one for its clauses and bindings:",
                edges.len(),
                if edges.len() == 1 { "" } else { "s" }
            ));
        }
        for edge in edges.iter().take(MAX_LISTED_EDGES) {
            self.edge_row(ui, view, edge);
        }
        if edges.len() > MAX_LISTED_EDGES {
            ui.weak(format!(
                "… {} more; open a rule to see its own edges.",
                edges.len() - MAX_LISTED_EDGES
            ));
        }
        let undecided: Vec<&Undecided> = web.undecided_in(scope).collect();
        for pair in undecided.iter().take(MAX_LISTED_UNDECIDED) {
            ui.weak(undecided_line(view, pair));
        }
        if undecided.len() > MAX_LISTED_UNDECIDED {
            ui.weak(format!(
                "… {} more undecided pairs.",
                undecided.len() - MAX_LISTED_UNDECIDED
            ));
        }
        if let Some(edge) = self
            .inspected(view, web)
            .filter(|edge| scope.holds(edge.from, edge.to))
        {
            ui.separator();
            draw_edge(ui, view, edge);
        }
        for cycle in &scope.cycles {
            draw_cycle(ui, view, cycle);
        }
        if scope.cycles.is_empty() && !edges.is_empty() {
            ui.small(if scope.complete() {
                "No static feedback cycle among the executable revisions."
            } else {
                "No feedback cycle was witnessed; the evidence is incomplete, so this does not say that none exists."
            });
        }
        ui.weak("Firings are not recorded in the pile, so observed firings are not shown; no cycle is claimed productive.");
    }

    /// A compact ring of rule marks and directed links: click a mark to open
    /// its rule, a link to inspect its edge. Marks: filled whole, open partly
    /// decoded, dashed refused. Links: dashed when structural only.
    fn draw_diagram(&mut self, ui: &mut Ui, view: &RulesView, web: &Web, scope: &Scope) {
        let nodes: Vec<usize> = scope
            .members
            .iter()
            .copied()
            .take(MAX_DIAGRAM_NODES)
            .collect();
        if nodes.is_empty() {
            return;
        }
        let inside: Vec<&Edge> = web
            .edges_in(scope)
            .filter(|edge| nodes.contains(&edge.from) && nodes.contains(&edge.to))
            .collect();
        let shown: Vec<&Edge> = inside.iter().copied().take(MAX_DIAGRAM_EDGES).collect();
        let width = ui.available_width();
        let radius = diagram::radius_for(nodes.len()).min((width / 2.0 - 70.0).max(40.0));
        let size = egui::vec2(width, 2.0 * (radius + diagram::LABEL + 24.0));
        let (response, painter) = ui.allocate_painter(size, egui::Sense::click());
        let rect = response.rect;
        let centre = rect.center();
        let points = diagram::ring(nodes.len(), centre, radius);
        let at = |rule: usize| {
            nodes
                .iter()
                .position(|&node| node == rule)
                .map(|k| points[k])
                .unwrap_or(centre)
        };
        let marks: Vec<diagram::Mark> = shown
            .iter()
            .map(|edge| {
                if edge.from == edge.to {
                    diagram::self_loop(at(edge.from), centre)
                } else {
                    diagram::link(at(edge.from), at(edge.to))
                }
            })
            .collect();
        let ink = ui.visuals().text_color();
        let hairline = ui.visuals().widgets.noninteractive.bg_stroke.color;
        let accent = GORBIE::themes::button_light_on();
        let font = egui::TextStyle::Small.resolve(ui.style());
        let key = |edge: &Edge| (view.rules[edge.from].revision, view.rules[edge.to].revision);

        // Links first, so marks sit on top of them.
        for (edge, mark) in shown.iter().zip(&marks) {
            let weight = if self.edge == Some(key(*edge)) {
                2.5
            } else {
                1.0
            };
            match *mark {
                diagram::Mark::Link { from, to } => graph::draw_link(
                    &painter,
                    from,
                    to,
                    1.0,
                    !edge.hypothetical,
                    Some(accent),
                    hairline,
                    weight,
                ),
                diagram::Mark::Loop { centre: middle } => {
                    let ring = graph::arc_points(middle, diagram::LOOP, 0.0, std::f32::consts::TAU);
                    let stroke = egui::Stroke::new(weight, hairline);
                    if edge.hypothetical {
                        painter.extend(egui::Shape::dashed_line(&ring, stroke, 3.0, 3.0));
                    } else {
                        painter.add(egui::Shape::line(ring, stroke));
                    }
                }
            }
        }
        let mut labels = Vec::with_capacity(nodes.len());
        for (k, &rule) in nodes.iter().enumerate() {
            let line = &view.rules[rule];
            let outline = if !line.executable() {
                graph::Stroke2::Dashed
            } else if !line.partial.is_empty() {
                graph::Stroke2::Open
            } else {
                graph::Stroke2::Filled
            };
            graph::draw_node(
                &painter,
                points[k],
                1.0,
                graph::Glyph::Circle,
                outline,
                ink,
                1,
            );
            let (pos, anchor) = diagram::label_at(points[k], centre);
            labels.push(painter.text(pos, anchor, tag(k), font.clone(), ink));
        }

        let node_at = |p: egui::Pos2| {
            labels
                .iter()
                .position(|label| label.expand(2.0).contains(p))
                .or_else(|| graph::pick(&points, p, graph::HIT))
        };
        if response.clicked() {
            if let Some(p) = response.interact_pointer_pos() {
                if let Some(k) = node_at(p) {
                    self.clicked = Some(Some(view.rules[nodes[k]].revision));
                } else if let Some(e) = diagram::pick(&marks, p, diagram::REACH) {
                    let chosen = key(shown[e]);
                    self.clicked_edge = Some(if self.edge == Some(chosen) {
                        None
                    } else {
                        Some(chosen)
                    });
                }
            }
        }
        let hover = response.hover_pos().and_then(|p| {
            node_at(p)
                .map(|k| {
                    format!(
                        "{} {} (click to open)",
                        tag(k),
                        view.rules[nodes[k]].title()
                    )
                })
                .or_else(|| {
                    diagram::pick(&marks, p, diagram::REACH)
                        .map(|e| format!("{} (click to inspect)", edge_line(view, shown[e])))
                })
        });
        if let Some(text) = hover {
            let _ = response.on_hover_text_at_pointer(text);
        }
        ui.small(
            nodes
                .iter()
                .enumerate()
                .map(|(k, &rule)| format!("{} = {}", tag(k), view.rules[rule].title()))
                .collect::<Vec<_>>()
                .join(" · "),
        );
        if scope.members.len() > nodes.len() {
            ui.weak(format!(
                "The diagram draws the first {} of {} analysed revisions; the list below names their edges.",
                nodes.len(),
                scope.members.len()
            ));
        }
        if inside.len() > shown.len() {
            ui.weak(format!(
                "The diagram draws {} of {} links.",
                shown.len(),
                inside.len()
            ));
        }
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
        for cause in &rule.refused {
            ui.colored_label(ui.visuals().warn_fg_color, format!("Refused: {cause}"));
        }
        for part in &rule.partial {
            ui.weak(format!("Not decoded here: {part}"));
        }
        for note in &rule.notes {
            ui.weak(note.as_str());
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
        let scopes: Vec<&Scope> = web
            .scopes
            .iter()
            .filter(|scope| scope.members.contains(&index))
            .collect();
        if scopes.is_empty() {
            ui.weak(
                "This revision is outside the analysed web: no current reading selects it while others are selected, \
                 or it lies beyond the web's bound.",
            );
            return;
        }
        if !rule.executable() {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                "The engine refuses this revision, so its edges are structural only and it is in no feedback cycle.",
            );
        }
        let edges: Vec<&Edge> = web.touching(index).collect();
        if edges.is_empty() {
            ui.small(if scopes.iter().all(|scope| scope.complete()) {
                "It feeds no analysed rule, and none feeds it."
            } else {
                "No edge was witnessed for it. The evidence is incomplete (see the rule list), so this does not say that none exists."
            });
        }
        for edge in edges.iter().take(MAX_LISTED_EDGES) {
            self.edge_row(ui, view, edge);
        }
        if edges.len() > MAX_LISTED_EDGES {
            ui.weak(format!("… {} more edges.", edges.len() - MAX_LISTED_EDGES));
        }
        for pair in web
            .undecided
            .iter()
            .filter(|pair| pair.from == index || pair.to == index)
            .take(MAX_LISTED_UNDECIDED)
        {
            ui.weak(undecided_line(view, pair));
        }
        if let Some(edge) = self
            .inspected(view, web)
            .filter(|edge| edge.from == index || edge.to == index)
        {
            ui.separator();
            draw_edge(ui, view, edge);
        }
        for scope in &scopes {
            for cycle in scope.cycles.iter().filter(|c| c.members.contains(&index)) {
                draw_cycle(ui, view, cycle);
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
    if !view.selected_missing.is_empty() {
        ui.colored_label(
            ui.visuals().warn_fg_color,
            format!(
                "Selected but with no rule facts in this collection: {}",
                view.selected_missing
                    .iter()
                    .take(8)
                    .map(|id| short(*id))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        );
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

/// A diagram mark's label: `R1`, `R2`, … in scope order.
fn tag(k: usize) -> String {
    format!("R{}", k + 1)
}

fn scope_title(scope: &Scope) -> String {
    match &scope.kind {
        ScopeKind::Reading(anchors) => format!(
            "Reading of {}",
            anchors
                .iter()
                .map(|anchor| short(*anchor))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        ScopeKind::WhatIf => "What-if: all listed revisions".to_owned(),
    }
}

fn scope_intro(scope: &Scope) -> String {
    match &scope.kind {
        ScopeKind::Reading(anchors) => format!(
            "Static analysis of what the current reading of {} selects ({} revision{} analysed): \
             an edge means one rule can conclude a fact another rule matches, not that it did.",
            anchors
                .iter()
                .map(|anchor| short(*anchor))
                .collect::<Vec<_>>()
                .join(", "),
            scope.members.len(),
            if scope.members.len() == 1 { "" } else { "s" }
        ),
        ScopeKind::WhatIf => "No current reading selects a revision, so this is a hypothetical what-if over all \
             listed revisions; no environment runs them together. An edge means one rule can conclude a fact \
             another matches, not that it did."
            .to_owned(),
    }
}

/// Everything this scope lost or left open, said before any edge is.
fn draw_gaps(ui: &mut Ui, view: &RulesView, scope: &Scope) {
    let mut gaps = Vec::new();
    if scope.missing > 0 {
        gaps.push(format!(
            "{} selected revision(s) have no rule facts in this collection and are not analysed.",
            scope.missing
        ));
    }
    if scope.unlisted > 0 {
        gaps.push(match scope.kind {
            ScopeKind::Reading(_) => format!(
                "{} selected revision(s) lie beyond the first {} listed and are not analysed.",
                scope.unlisted,
                projection::MAX_RULES
            ),
            ScopeKind::WhatIf => format!(
                "{} revision(s) beyond the first {} listed are not analysed.",
                scope.unlisted,
                projection::MAX_RULES
            ),
        });
    }
    if scope.beyond > 0 {
        gaps.push(format!(
            "{} revision(s) beyond the web's bound of {} are not analysed.",
            scope.beyond,
            web::MAX_WEB_RULES
        ));
    }
    if !scope.partial.is_empty() {
        gaps.push(format!(
            "Only partly decoded here: {}; their edges may be incomplete.",
            scope
                .partial
                .iter()
                .map(|&rule| view.rules[rule].title())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if scope.undecided > 0 {
        gaps.push(format!(
            "{} pair(s) were left undecided by the bounded search.",
            scope.undecided
        ));
    }
    if gaps.is_empty() {
        return;
    }
    for gap in gaps {
        ui.colored_label(ui.visuals().warn_fg_color, gap);
    }
    ui.weak("So a missing edge or cycle here is not evidence that none exists, and termination is not stated.");
}

fn undecided_line(view: &RulesView, pair: &Undecided) -> String {
    format!(
        "Undecided: {} → {} ({})",
        view.rules[pair.from].title(),
        view.rules[pair.to].title(),
        match pair.why {
            Unsearched::Bound =>
                "the pair's search bound was reached before any consistent assignment",
            Unsearched::Budget => "the web's total search budget ran out",
        }
    )
}

fn edge_line(view: &RulesView, edge: &Edge) -> String {
    let (from, to) = (view.rules[edge.from].title(), view.rules[edge.to].title());
    let arrow = if edge.from == edge.to {
        format!("{from} can feed itself")
    } else {
        format!("{from} can feed {to}")
    };
    format!(
        "{arrow}: {} maximal overlap{}{}{}",
        edge.overlaps.len(),
        if edge.overlaps.len() == 1 { "" } else { "s" },
        if edge.bounded {
            " (search bounded; more may exist)"
        } else {
            ""
        },
        if edge.hypothetical {
            " (structural only: a refused revision)"
        } else {
            ""
        }
    )
}

fn condition_text(producer: &RuleLine, binding: &Binding, condition: Condition) -> String {
    let terms = binding
        .producer
        .iter()
        .map(|&term| producer.terms[term].label.as_str())
        .collect::<Vec<_>>();
    let literals = binding
        .literals
        .iter()
        .map(|value| value.text.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let title = producer.title();
    match condition {
        Condition::ProducerEqual => format!(
            "only when one firing of {title} binds {} to the same value",
            terms.join(" and ")
        ),
        Condition::DerivedLiteral => format!(
            "{{{literals}}} would still have to equal the identity {title} constructs at runtime for {}: \
             structurally compatible, not decided here",
            terms.join(" = ")
        ),
        Condition::InputLiteral => format!(
            "only for firings of {title} whose {} is one of {{{literals}}}",
            terms.join(" = ")
        ),
        Condition::NarrowedLiteral => {
            format!("only {title}'s alternatives {{{literals}}} fit")
        }
    }
}

fn draw_edge(ui: &mut Ui, view: &RulesView, edge: &Edge) {
    let (producer, consumer) = (&view.rules[edge.from], &view.rules[edge.to]);
    ui.label(RichText::new(edge_line(view, edge)).strong());
    if edge.hypothetical {
        let mut refused: Vec<String> = Vec::new();
        for rule in [producer, consumer] {
            if !rule.executable() && !refused.contains(&rule.title()) {
                refused.push(rule.title());
            }
        }
        ui.colored_label(
            ui.visuals().warn_fg_color,
            format!(
                "Structural only: the engine refuses {}, so this edge cannot occur as stored.",
                refused.join(" and ")
            ),
        );
    }
    if edge.bounded {
        ui.weak("The search stopped at its bound; further maximal overlaps may exist.");
    }
    ui.weak(format!(
        "Structurally, {} can conclude facts of these shapes. Whether it fires, and with which values, depends on its own inputs.",
        producer.title()
    ));
    let total = consumer.clauses(ClauseKind::Matches).count();
    for (n, overlap) in edge.overlaps.iter().enumerate() {
        ui.small(format!(
            "  overlap {}: {} of {} matches clause{} met",
            n + 1,
            overlap.pairs.len(),
            total,
            if total == 1 { "" } else { "s" }
        ));
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
            for condition in &binding.conditions {
                ui.weak(format!(
                    "      {}",
                    condition_text(producer, binding, *condition)
                ));
            }
        }
        let shared: Vec<usize> = overlap
            .bindings
            .iter()
            .flat_map(|binding| binding.consumer.iter().copied())
            .collect();
        for &matched in &overlap.remaining {
            let clause = &consumer.clauses[matched];
            let mut touches: Vec<&str> = Vec::new();
            for term in [clause.subject, clause.object] {
                let label = consumer.terms[term].label.as_str();
                if shared.contains(&term) && !touches.contains(&label) {
                    touches.push(label);
                }
            }
            ui.monospace(format!(
                "    still needs {} from other facts{}",
                consumer.clause_text(clause),
                if touches.is_empty() {
                    String::new()
                } else {
                    format!(", sharing {} with this overlap", touches.join(" and "))
                }
            ));
        }
        for constraint in &consumer.constraints {
            ui.monospace(format!(
                "    still needs STRING_CONTAINS({}, {}) to hold on the bound strings; content is not read here",
                consumer.terms[constraint.container].display(),
                consumer.terms[constraint.needle].display()
            ));
        }
        if overlap.unconditional(consumer) {
            ui.small(format!(
                "    no remaining conditions: every matches clause of {} is met by these conclusions alone (the producer must still fire)",
                consumer.title()
            ));
        }
    }
}

fn draw_cycle(ui: &mut Ui, view: &RulesView, cycle: &Cycle) {
    let title = |rule: usize| view.rules[rule].title();
    if cycle.members.len() == 1 {
        ui.label(format!(
            "Static feedback cycle: {} can feed itself",
            title(cycle.members[0])
        ));
    } else {
        ui.label(format!(
            "Static feedback cycle among {{{}}}",
            cycle
                .members
                .iter()
                .map(|&rule| title(rule))
                .collect::<Vec<_>>()
                .join(", ")
        ));
        let mut walk: Vec<String> = cycle.walk.iter().map(|&rule| title(rule)).collect();
        if let Some(&first) = cycle.walk.first() {
            walk.push(title(first));
        }
        ui.small(format!("for example {}", walk.join(" → ")));
    }
    ui.weak(cycle_note(cycle));
}

fn cycle_note(cycle: &Cycle) -> &'static str {
    if cycle.constructs {
        "A rule in this cycle constructs new identities: it may settle or keep growing. This viewer does not decide termination."
    } else if cycle.settles {
        "No rule in this cycle constructs a new identity, so it can only relate values already present: it settles whenever its inputs are finite."
    } else {
        "No rule in this witnessed cycle constructs a new identity, but this scope's evidence is incomplete (see above), so termination is not stated."
    }
}
