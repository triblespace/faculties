//! Colony activity from the selected workspace's frozen evidence.
use crate::care::format_time;
use crate::spawn::colony::{
    colony_view, ColonyOptions, ColonyRole, ColonyRoles, ColonyView, GoalLine, ParentSource,
    ParentView, WorkerLine,
};
use crate::widgets::{DatasetRevision, DatasetView, SourceKey, WidgetContext};
use egui::{RichText, Ui};
use std::time::Duration;
use GORBIE::dataflow::ComputedState;
use GORBIE::prelude::CardCtx;

pub(crate) const INPUTS: [SourceKey; 4] = [
    SourceKey::Spawn,
    SourceKey::Compass,
    SourceKey::Messages,
    SourceKey::Relations,
];

#[derive(Default)]
pub struct ColonyViewer {
    result: ComputedState<
        Option<(
            [(Option<DatasetRevision>, bool); 4],
            Result<ColonyView, String>,
        )>,
    >,
}

impl ColonyViewer {
    pub fn render(&mut self, ctx: &mut CardCtx<'_>, sources: WidgetContext<'_>) {
        ctx.section("Colony", |ctx| {
            let revisions = INPUTS.map(|key| (sources.revision(key), sources.missing_route(key)));
            self.result.poll();
            let current = self
                .result
                .value()
                .as_ref()
                .filter(|(cached, _)| *cached == revisions);
            if current.is_none() && !self.result.is_running() {
                let inputs = INPUTS.map(|key| {
                    sources
                        .owned_dataset(key)
                        .ok_or(if sources.missing_route(key) {
                            "route missing from this workspace"
                        } else {
                            "routed evidence unavailable"
                        })
                });
                self.result.spawn(ctx.ui_mut().ctx(), move || {
                    let views = inputs.each_ref().map(|input| {
                        input
                            .as_ref()
                            .map_err(|reason| *reason)
                            .and_then(|dataset| dataset.view())
                    });
                    Some((revisions, read_colony(views)))
                });
            }
            ctx.with_padding(12, |ctx| {
                let ui = ctx.ui_mut();
                for (key, label) in
                    INPUTS
                        .into_iter()
                        .zip(["Spawn", "Compass", "Messages", "Relations"])
                {
                    if sources.missing_route(key) {
                        ui.weak(format!("{label} route missing from this workspace"));
                    } else if !sources.contains(key) {
                        ui.weak(format!("{label} unavailable in this workspace"));
                    }
                }
                match self.result.value() {
                    Some((cached, Ok(view))) if *cached == revisions => draw(ui, view),
                    Some((cached, Err(error))) if *cached == revisions => {
                        ui.label(error);
                    }
                    _ => {
                        ui.label("Reading colony evidence…");
                        ui.ctx().request_repaint_after(Duration::from_millis(250));
                    }
                }
            });
        });
    }
}

fn read_colony(views: [Result<DatasetView<'_>, &str>; 4]) -> Result<ColonyView, String> {
    use crate::schemas::compass::board;
    use triblespace::core::metadata;
    let reader = views
        .iter()
        .find_map(|view| view.as_ref().ok().map(|dataset| dataset.reader))
        .ok_or_else(|| "Current activity unknown. No Colony source is readable.".to_owned())?;
    let [spawn, compass, message, relations] = views;
    let roles = ColonyRoles {
        spawn: spawn.map(|view| view.facts),
        message: message.map(|view| view.facts),
        relations: relations.map(|view| view.facts),
        compass: compass.map(|view| view.facts),
        compass_status: compass.as_ref().map_err(|reason| *reason).and_then(|view| {
            view.lww_register(board::status_of.id(), metadata::created_at.id())
                .ok_or("current Compass status unavailable")
        }),
    };
    let at = crate::clock::point_now().map_err(|error| error.to_string())?;
    Ok(colony_view(
        roles,
        reader,
        at,
        None,
        &ColonyOptions::default(),
    ))
}

fn draw(ui: &mut Ui, view: &ColonyView) {
    ui.small(format!(
        "Local evidence observed {}",
        format_time(view.observed_at)
    ));
    for (role, reason) in &view.unavailable {
        if (*role == ColonyRole::CompassStatus && view.available(ColonyRole::Compass))
            || !matches!(
                reason.as_str(),
                "route missing from this workspace"
                    | "routed evidence unavailable"
                    | "current Compass status unavailable"
            )
        {
            ui.weak(reason);
        }
    }
    for reason in &view.unreadable {
        ui.weak(reason);
    }
    if view.parents.is_empty() && view.unparented.is_empty() && view.unanswered.is_empty() {
        ui.label("Current activity unknown. No parent or worker evidence is visible.");
    }
    for parent in &view.parents {
        ui.separator();
        ui.push_id((&parent.label, parent.persona), |ui| {
            draw_parent(ui, parent)
        });
    }
    if !view.unparented.is_empty() {
        ui.separator();
        ui.strong("Workers without a recorded parent");
        for worker in &view.unparented {
            draw_worker(ui, worker, None);
        }
    }
    if !view.unanswered.is_empty() {
        ui.separator();
        ui.strong("Requests without a recorded outcome");
        for request in &view.unanswered {
            ui.label(format!(
                "{} · {}",
                request.name.as_deref().unwrap_or("Unnamed request"),
                request.host
            ));
            if let Some(at) = request.requested_at {
                ui.small(format!("Spawn · {}", format_time(at)));
            }
        }
    }
}

fn draw_parent(ui: &mut Ui, parent: &ParentView) {
    ui.horizontal_wrapped(|ui| {
        ui.heading(&parent.label);
        if parent.shown_because.contains(&ParentSource::Roster) {
            ui.weak("Relations roster");
        } else if parent.shown_because.contains(&ParentSource::Spawner) {
            ui.weak("Recorded spawner");
        }
    });
    if let Some(at) = parent.last_message {
        ui.weak(format!("Last outgoing message · {}", format_time(at)));
    }
    if let Some(at) = parent.last_spawn {
        ui.weak(format!("Last spawned worker · {}", format_time(at)));
    }
    if parent.doing.is_empty() && parent.asks_jp.is_empty() {
        ui.label("Current activity unknown");
    }
    for goal in &parent.doing {
        draw_goal(ui, goal, "Compass · doing");
    }
    for goal in &parent.asks_jp {
        draw_goal(ui, goal, "Requested from JP");
    }
    for worker in &parent.open {
        draw_worker(ui, worker, Some(parent));
    }
    for worker in &parent.unreviewed {
        draw_worker(ui, worker, Some(parent));
        ui.small("Message · report unread by parent");
    }
    if parent.settled > 0 {
        ui.small(format!(
            "{} ended runs without a visible unread report",
            parent.settled
        ));
    }
    if parent.review_unknown > 0 {
        ui.small(format!(
            "{} ended runs · report acknowledgment unknown",
            parent.review_unknown
        ));
    }
    for reason in &parent.unreadable {
        ui.weak(reason);
    }
}

fn draw_goal(ui: &mut Ui, goal: &GoalLine, label: &str) {
    ui.label(RichText::new(label).strong());
    ui.label(&goal.title);
    if let Some(at) = &goal.last_note {
        ui.small(format!("Latest Compass note · {at}"));
    }
}

fn draw_worker(ui: &mut Ui, worker: &WorkerLine, parent: Option<&ParentView>) {
    ui.indent(worker.run, |ui| {
        ui.strong(&worker.name);
        for goal in &worker.goal {
            let title = parent.and_then(|p| {
                p.doing
                    .iter()
                    .chain(&p.asks_jp)
                    .find(|line| line.goal == *goal)
            });
            match title {
                Some(line) => {
                    ui.small(format!("Assigned goal · {}", line.title));
                }
                None => {
                    ui.small(format!("Assigned goal · {goal:x}"));
                }
            }
        }
        if let Some(at) = worker.finished {
            ui.label(format!("Run ended · {}", worker.end));
            ui.small(format!("Spawn · {}", format_time(at)));
        } else if let Some(at) = worker.started {
            ui.label("Started · current run state unknown");
            ui.small(format!("Spawn · {}", format_time(at)));
        } else {
            ui.label("Run state unknown");
        }
    });
}
