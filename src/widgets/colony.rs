//! Colony activity from the selected workspace's frozen evidence.
use crate::care::format_time;
use crate::spawn::colony::{
    colony_view, ColonyOptions, ColonyRole, ColonyRoles, ColonyView, GoalLine, ParentSource,
    ParentView, WorkerLine,
};
use crate::spawn::lived::{self, Body, Direction, History, Life, Turn};
use crate::spawn::TextHandle;
use crate::widgets::storage::LoadedDataset;
use crate::widgets::{DatasetRevision, DatasetView, SourceKey, WidgetContext};
use anybytes::View;
use egui::{RichText, Ui};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use triblespace::core::repo::BlobStoreGet;
use triblespace::prelude::Id;
use GORBIE::dataflow::ComputedState;
use GORBIE::prelude::CardCtx;

pub(crate) const INPUTS: [SourceKey; 4] = [
    SourceKey::Spawn,
    SourceKey::Goals,
    SourceKey::Messages,
    SourceKey::Relations,
];

/// What the Colony target observes: the overview's inputs, and the Archive a
/// run's lived history is read from when someone opens it. The overview
/// never waits on the Archive.
pub(crate) const SOURCES: [SourceKey; 5] = [
    SourceKey::Spawn,
    SourceKey::Goals,
    SourceKey::Messages,
    SourceKey::Relations,
    SourceKey::Archive,
];

/// What a lived history reads: the run's Spawn facts and the Archive.
const LIFE_INPUTS: [SourceKey; 2] = [SourceKey::Spawn, SourceKey::Archive];
/// Turns shown per page.
const TURNS_PER_PAGE: usize = 40;
/// Bytes of one text shown per page, and of the prepared opening input.
const TEXT_PAGE: usize = 16 * 1024;
const OPENING_PAGE: usize = 32 * 1024;

#[derive(Default)]
pub struct ColonyViewer {
    result: ComputedState<
        Option<(
            [(Option<DatasetRevision>, bool); 4],
            Result<ColonyView, String>,
        )>,
    >,
    /// The lived history of each run someone opened, by run.
    lives: HashMap<Id, LifePanel>,
}

type LifeRevisions = [(Option<DatasetRevision>, bool); 2];

/// One opened run: its history, read once per observation, and where its
/// reader is in it. Ephemeral UI state, replaced when the inputs change.
#[derive(Default)]
struct LifePanel {
    life: ComputedState<Option<(LifeRevisions, Result<Arc<LifeShown>, String>)>>,
    /// The whole shared conversation, when the reader asked for it.
    whole: bool,
    conversation: ComputedState<Option<(LifeRevisions, Result<Option<Arc<Vec<Id>>>, String>)>>,
    opening: ComputedState<Option<(LifeRevisions, Vec<Result<Arc<str>, String>>)>>,
    /// The page of each prepared input, by its position on the run.
    opening_pages: HashMap<usize, usize>,
    page: usize,
    turns: ComputedState<Option<((LifeRevisions, bool, usize), Arc<Vec<Turn>>)>>,
    text_pages: HashMap<(Id, usize), usize>,
}

/// A run's history as read: the facts' answer, the nodes withheld after a
/// recorded classifier stop, and whether the archive opens with the
/// prepared input.
struct LifeShown {
    life: Life,
    /// The nodes shown for its own or candidate history.
    own: Arc<Vec<Id>>,
    /// Nodes a recorded classifier stop followed. Ephemeral UI cache of one
    /// observation, dropped with the panel when the inputs change.
    stopped: HashSet<Id>,
    agrees: Option<bool>,
}

/// The two datasets a history reads, kept for its background reads.
#[derive(Clone)]
struct LifeSources {
    revisions: LifeRevisions,
    spawn: Result<LoadedDataset, &'static str>,
    archive: Result<LoadedDataset, &'static str>,
}

impl LifeSources {
    fn of(sources: &WidgetContext<'_>) -> Self {
        let owned = |key| {
            sources
                .owned_dataset(key)
                .ok_or(if sources.missing_route(key) {
                    "route missing from this workspace"
                } else {
                    "routed evidence unavailable"
                })
        };
        Self {
            revisions: LIFE_INPUTS.map(|key| (sources.revision(key), sources.missing_route(key))),
            spawn: owned(SourceKey::Spawn),
            archive: owned(SourceKey::Archive),
        }
    }
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
                        .zip(["Spawn", "Goals", "Messages", "Relations"])
                {
                    if sources.missing_route(key) {
                        ui.weak(format!("{label} route missing from this workspace"));
                    } else if !sources.contains(key) {
                        ui.weak(format!("{label} unavailable in this workspace"));
                    }
                }
                let life_sources = LifeSources::of(&sources);
                let mut lives = Lives {
                    panels: &mut self.lives,
                    sources: &life_sources,
                };
                match self.result.value() {
                    Some((cached, Ok(view))) if *cached == revisions => draw(ui, view, &mut lives),
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
    let [spawn, goals, message, relations] = views;
    let roles = ColonyRoles {
        spawn: spawn.map(|view| view.facts),
        message: message.map(|view| view.facts),
        relations: relations.map(|view| view.facts),
        goals: goals.map(|view| view.facts),
        goals_status: goals.as_ref().map_err(|reason| *reason).and_then(|view| {
            view.lww_register(board::status_of.id(), metadata::created_at.id())
                .ok_or("current Goals status unavailable")
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

/// The opened runs and what their histories read.
struct Lives<'a> {
    panels: &'a mut HashMap<Id, LifePanel>,
    sources: &'a LifeSources,
}

fn draw(ui: &mut Ui, view: &ColonyView, lives: &mut Lives<'_>) {
    ui.small(format!(
        "Local evidence observed {}",
        format_time(view.observed_at)
    ));
    for (role, reason) in &view.unavailable {
        if (*role == ColonyRole::GoalsStatus && view.available(ColonyRole::Goals))
            || !matches!(
                reason.as_str(),
                "route missing from this workspace"
                    | "routed evidence unavailable"
                    | "current Goals status unavailable"
            )
        {
            ui.weak(reason);
        }
    }
    for reason in &view.unreadable {
        ui.weak(reason);
    }
    if view.parents.is_empty() && view.unparented.is_empty() {
        ui.label("Current activity unknown. No parent or worker evidence is visible.");
    }
    for parent in &view.parents {
        ui.separator();
        ui.push_id((&parent.label, parent.persona), |ui| {
            draw_parent(ui, parent, lives)
        });
    }
    if !view.unparented.is_empty() {
        ui.separator();
        ui.strong("Workers without a recorded parent");
        for worker in &view.unparented {
            draw_worker(ui, worker, None, lives);
        }
    }
}

fn draw_parent(ui: &mut Ui, parent: &ParentView, lives: &mut Lives<'_>) {
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
        draw_goal(ui, goal, "Goals · doing");
    }
    for goal in &parent.asks_jp {
        draw_goal(ui, goal, "Requested from JP");
    }
    for worker in &parent.open {
        draw_worker(ui, worker, Some(parent), lives);
    }
    for worker in &parent.unreviewed {
        draw_worker(ui, worker, Some(parent), lives);
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
    if !parent.ended.is_empty() {
        egui::CollapsingHeader::new(format!("Ended runs ({})", parent.ended.len()))
            .id_salt(("ended", &parent.label))
            .show(ui, |ui| {
                for worker in &parent.ended {
                    draw_worker(ui, worker, Some(parent), lives);
                }
            });
    }
    for reason in &parent.unreadable {
        ui.weak(reason);
    }
}

fn draw_goal(ui: &mut Ui, goal: &GoalLine, label: &str) {
    ui.label(RichText::new(label).strong());
    ui.label(&goal.title);
    if let Some(at) = &goal.last_note {
        ui.small(format!("Latest note · {at}"));
    }
}

fn draw_worker(
    ui: &mut Ui,
    worker: &WorkerLine,
    parent: Option<&ParentView>,
    lives: &mut Lives<'_>,
) {
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
        egui::CollapsingHeader::new("What it lived")
            .id_salt(("lived", worker.run))
            .show(ui, |ui| {
                let panel = lives.panels.entry(worker.run).or_default();
                draw_life(ui, worker.run, panel, lives.sources);
            });
    });
}

fn read_blob(reader: &impl BlobStoreGet, handle: TextHandle) -> Result<Arc<str>, String> {
    reader
        .get::<View<str>, _>(handle)
        .map(|text| Arc::from(text.as_ref()))
        .map_err(|_| "its text is not resident in this snapshot".to_owned())
}

/// Read what `run` lived from the two datasets, on a background worker.
fn read_life(sources: &LifeSources, run: Id) -> Result<LifeShown, String> {
    let spawn = sources
        .spawn
        .as_ref()
        .map_err(|why| format!("Spawn {why}."))?
        .view()
        .map_err(str::to_owned)?;
    // An Archive that cannot be read here still leaves the opening input.
    let archive: Result<DatasetView<'_>, String> = match sources.archive.as_ref() {
        Ok(dataset) => dataset.view().map_err(str::to_owned),
        Err(why) => Err((*why).to_owned()),
    };
    let life = lived::life(
        spawn.facts,
        spawn.reader,
        archive
            .as_ref()
            .map(|view| view.facts)
            .map_err(String::as_str),
        run,
    )
    .map_err(|error| format!("{error:#}"))?;
    let archive = archive.ok();
    let (stopped, agrees) = match (&archive, &life.history) {
        (Some(view), History::Own { nodes, .. } | History::Candidate { nodes, .. }) => {
            // Only handles are compared here: no body, and no prepared
            // input, is read before someone opens it.
            let agrees =
                lived::opening_agrees(&life.opening, lived::first_received(view.facts, nodes));
            (
                crate::archive::classifier_stopped(view.facts)
                    .into_iter()
                    .collect(),
                agrees,
            )
        }
        (Some(view), History::Shared { .. }) => (
            crate::archive::classifier_stopped(view.facts)
                .into_iter()
                .collect(),
            None,
        ),
        _ => (HashSet::new(), None),
    };
    let own = Arc::new(match &life.history {
        History::Own { nodes, .. } | History::Candidate { nodes, .. } => nodes.clone(),
        _ => Vec::new(),
    });
    Ok(LifeShown {
        life,
        own,
        stopped,
        agrees,
    })
}

fn draw_life(ui: &mut Ui, run: Id, panel: &mut LifePanel, sources: &LifeSources) {
    let revisions = sources.revisions;
    let waiting = |ui: &mut Ui, what: &str| {
        ui.weak(format!("Reading {what}…"));
        ui.ctx().request_repaint_after(Duration::from_millis(250));
    };
    panel.life.poll();
    let current = panel
        .life
        .value()
        .as_ref()
        .is_some_and(|(cached, _)| *cached == revisions);
    if !current {
        if !panel.life.is_running() {
            // The observation changed: everything read from the old one goes.
            if panel.life.value().is_some() {
                *panel = LifePanel::default();
            }
            let sources = sources.clone();
            panel.life.spawn(ui.ctx(), move || {
                Some((revisions, read_life(&sources, run).map(Arc::new)))
            });
        }
        waiting(ui, "what this run lived");
        return;
    }
    let shown = match panel.life.value() {
        Some((_, Ok(shown))) => shown.clone(),
        Some((_, Err(error))) => {
            ui.label(error.as_str());
            return;
        }
        None => return,
    };
    draw_opening(ui, run, panel, sources, &shown);
    ui.add_space(4.0);
    ui.label(RichText::new("Turns").strong());
    let nodes: Arc<Vec<Id>> = match &shown.life.history {
        History::Own { basis, .. } => {
            ui.small(basis.as_str());
            shown.own.clone()
        }
        History::Candidate { basis, .. } => {
            ui.label("Candidate history: linked by matching facts, not by a recorded edge.");
            ui.small(basis.as_str());
            shown.own.clone()
        }
        History::Shared { locator, runs, why } => {
            ui.label("This run's own turns are not separable here.");
            ui.small(why.as_str());
            let lives: Vec<String> = runs
                .iter()
                .map(|id| {
                    let short = format!("{id:x}");
                    if *id == run {
                        format!("{} (this run)", &short[..8])
                    } else {
                        short[..8].to_owned()
                    }
                })
                .collect();
            ui.small(format!(
                "Runs naming {locator}, oldest first: {}",
                lives.join(", ")
            ));
            if !panel.whole {
                if ui
                    .button(format!("Read the whole conversation {locator}, every life"))
                    .clicked()
                {
                    panel.whole = true;
                }
                return;
            }
            ui.label(
                RichText::new(format!(
                    "The whole conversation {locator}: every life's turns, not this run's alone"
                ))
                .strong(),
            );
            panel.conversation.poll();
            let current = panel
                .conversation
                .value()
                .as_ref()
                .filter(|(cached, _)| *cached == revisions);
            match current {
                Some((_, Ok(Some(nodes)))) => nodes.clone(),
                Some((_, Ok(None))) => {
                    ui.label(format!(
                        "This workspace's Archive holds no node of {locator}."
                    ));
                    return;
                }
                Some((_, Err(error))) => {
                    ui.label(error.as_str());
                    return;
                }
                None => {
                    if !panel.conversation.is_running() {
                        let sources = sources.clone();
                        let locator = locator.clone();
                        panel.conversation.spawn(ui.ctx(), move || {
                            let read = (|| {
                                let archive = sources
                                    .archive
                                    .as_ref()
                                    .map_err(|why| format!("Archive {why}."))?
                                    .view()
                                    .map_err(str::to_owned)?;
                                lived::conversation(archive.facts, &locator)
                                    .map(|nodes| nodes.map(Arc::new))
                                    .map_err(|error| format!("{error:#}"))
                            })();
                            Some((revisions, read))
                        });
                    }
                    waiting(ui, "the whole conversation");
                    return;
                }
            }
        }
        History::Empty(why) => {
            ui.label("Known empty.");
            ui.small(why.as_str());
            return;
        }
        History::NotArchived(why) => {
            ui.label("No archived history here yet.");
            ui.small(why.as_str());
            return;
        }
        History::Unlinked(why) => {
            ui.label("No recorded link to archived history.");
            ui.small(why.as_str());
            return;
        }
        History::Unavailable(why) => {
            ui.label("Archive history unavailable.");
            ui.small(why.as_str());
            return;
        }
    };
    draw_turns(ui, panel, sources, &shown, &nodes);
}

fn draw_opening(
    ui: &mut Ui,
    run: Id,
    panel: &mut LifePanel,
    sources: &LifeSources,
    shown: &LifeShown,
) {
    let revisions = sources.revisions;
    let opening = &shown.life.opening;
    if opening.is_empty() {
        ui.weak("This run records no prepared opening input.");
        return;
    }
    egui::CollapsingHeader::new("Prepared opening input")
        .id_salt(("opening", run))
        .show(ui, |ui| {
            ui.small(
                "The whole input this run's launch prepared and fed its agent. It is no \
                 evidence that every byte reached the model.",
            );
            match shown.agrees {
                Some(true) => {
                    ui.small("The archived conversation opens with exactly this text.");
                }
                Some(false) => {
                    ui.small(
                        "The first input the provider recorded is a different text; both are \
                         kept, the recorded one is the first turn below.",
                    );
                }
                None => {}
            }
            panel.opening.poll();
            let current = panel
                .opening
                .value()
                .as_ref()
                .filter(|(cached, _)| *cached == revisions)
                .map(|(_, texts)| texts.clone());
            let Some(texts) = current else {
                if !panel.opening.is_running() {
                    let sources = sources.clone();
                    let handles = opening.clone();
                    panel.opening.spawn(ui.ctx(), move || {
                        let texts = match sources.spawn.as_ref() {
                            Ok(dataset) => match dataset.view() {
                                Ok(view) => handles
                                    .iter()
                                    .map(|handle| read_blob(view.reader, *handle))
                                    .collect(),
                                Err(why) => vec![Err(why.to_owned())],
                            },
                            Err(why) => vec![Err(format!("Spawn {why}."))],
                        };
                        Some((revisions, texts))
                    });
                }
                ui.weak("Loading the prepared input…");
                ui.ctx().request_repaint_after(Duration::from_millis(250));
                return;
            };
            if texts.len() > 1 {
                ui.small(format!(
                    "{} prepared inputs are recorded on this run; each is shown.",
                    texts.len()
                ));
            }
            for (index, text) in texts.iter().enumerate() {
                match text {
                    Err(why) => {
                        ui.weak(format!("Prepared input {}: {why}.", index + 1));
                    }
                    Ok(text) => {
                        let page = panel.opening_pages.entry(index).or_default();
                        text_pager(ui, ("opening-text", run, index), text, OPENING_PAGE, page);
                        ui.small("End of the prepared opening input.")
                            .on_hover_text(
                                "Each page shows the stored bytes exactly; nothing is summarized.",
                            );
                    }
                }
            }
        });
}

/// The largest char boundary of `text` at or below `at`.
fn floor_boundary(text: &str, at: usize) -> usize {
    let mut at = at.min(text.len());
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// One page of `text`, `size` bytes at most, with a pager when it has more.
fn text_pager(ui: &mut Ui, salt: impl std::hash::Hash, text: &str, size: usize, page: &mut usize) {
    let len = text.len();
    if len <= size {
        ui.add(egui::Label::new(RichText::new(text).monospace()).wrap());
        return;
    }
    let pages = len.div_ceil(size);
    *page = (*page).min(pages - 1);
    let start = floor_boundary(text, *page * size);
    let end = floor_boundary(text, (*page + 1) * size);
    ui.push_id(salt, |ui| {
        ui.horizontal_wrapped(|ui| {
            if ui.add_enabled(*page > 0, egui::Button::new("◀")).clicked() {
                *page -= 1;
            }
            ui.small(format!(
                "bytes {start}–{end} of {len} · page {} of {pages}",
                *page + 1
            ));
            if ui
                .add_enabled(*page + 1 < pages, egui::Button::new("▶"))
                .clicked()
            {
                *page += 1;
            }
        });
        ui.add(egui::Label::new(RichText::new(&text[start..end]).monospace()).wrap());
    });
}

fn draw_turns(
    ui: &mut Ui,
    panel: &mut LifePanel,
    sources: &LifeSources,
    shown: &Arc<LifeShown>,
    nodes: &Arc<Vec<Id>>,
) {
    let revisions = sources.revisions;
    if nodes.is_empty() {
        ui.label("No turns.");
        return;
    }
    let pages = nodes.len().div_ceil(TURNS_PER_PAGE);
    panel.page = panel.page.min(pages - 1);
    // The pager moves the page first; the slice and its cache key are both
    // taken from the page it leaves, so a read never lands under another page.
    ui.horizontal_wrapped(|ui| {
        let shown_first = panel.page * TURNS_PER_PAGE;
        let shown_last = (shown_first + TURNS_PER_PAGE).min(nodes.len());
        if ui
            .add_enabled(panel.page > 0, egui::Button::new("◀ Earlier"))
            .clicked()
        {
            panel.page -= 1;
        }
        ui.small(format!(
            "Turns {}–{shown_last} of {}",
            shown_first + 1,
            nodes.len()
        ));
        if ui
            .add_enabled(panel.page + 1 < pages, egui::Button::new("Later ▶"))
            .clicked()
        {
            panel.page += 1;
        }
    });
    let first = panel.page * TURNS_PER_PAGE;
    let last = (first + TURNS_PER_PAGE).min(nodes.len());
    let key = (revisions, panel.whole, panel.page);
    panel.turns.poll();
    let current = panel
        .turns
        .value()
        .as_ref()
        .filter(|(cached, _)| *cached == key)
        .map(|(_, turns)| turns.clone());
    let Some(turns) = current else {
        if !panel.turns.is_running() {
            let sources = sources.clone();
            let shown = shown.clone();
            let page: Vec<Id> = nodes[first..last].to_vec();
            panel.turns.spawn(ui.ctx(), move || {
                let turns = match sources.archive.as_ref().map(|dataset| dataset.view()) {
                    Ok(Ok(view)) => lived::turns(view.facts, view.reader, &page, |node| {
                        shown.stopped.contains(&node)
                    }),
                    _ => Vec::new(),
                };
                Some((key, Arc::new(turns)))
            });
        }
        ui.weak("Reading these turns…");
        ui.ctx().request_repaint_after(Duration::from_millis(250));
        return;
    };
    let position: HashMap<Id, usize> = nodes
        .iter()
        .enumerate()
        .map(|(index, node)| (*node, index + 1))
        .collect();
    let number = |node: &Id| {
        position
            .get(node)
            .map(|index| format!("#{index}"))
            .unwrap_or_else(|| format!("{node:x}"))
    };
    let mut previous = None;
    for (offset, turn) in turns.iter().enumerate() {
        if previous != Some(turn.direction) {
            ui.add_space(6.0);
            let who = match turn.direction {
                Direction::Received => "Received",
                Direction::Produced => "Produced",
                Direction::Unknown => "Mixed or unknown direction",
            };
            let roles = if turn.roles.is_empty() {
                String::new()
            } else {
                format!(" · {}", turn.roles.join(", "))
            };
            ui.label(RichText::new(format!("{who}{roles}")).strong());
            previous = Some(turn.direction);
        }
        ui.push_id(turn.node, |ui| {
            ui.indent("turn", |ui| {
                let index = first + offset + 1;
                if turn.withheld {
                    ui.weak(format!(
                        "#{index} [withheld: a classifier stop followed this]"
                    ));
                    return;
                }
                let mut links = Vec::new();
                for call in &turn.answers {
                    links.push(format!("answers call {}", number(call)));
                }
                for result in &turn.answered_by {
                    links.push(format!("answered by {}", number(result)));
                }
                for (part_index, part) in turn.parts.iter().enumerate() {
                    let head = if part_index == 0 && !links.is_empty() {
                        format!("#{index} {} · {}", part.kind, links.join(" · "))
                    } else {
                        format!("#{index} {}", part.kind)
                    };
                    ui.small(head);
                    match &part.body {
                        Body::Text(text) => {
                            let page = panel.text_pages.entry((turn.node, part_index)).or_default();
                            text_pager(ui, part_index, text, TEXT_PAGE, page);
                        }
                        Body::Missing(why) | Body::Other(why) => {
                            ui.weak(why.as_str());
                        }
                    }
                }
                if turn.parts.is_empty() {
                    ui.weak(format!("#{index} holds no part this view can read"));
                }
            });
        });
    }
    if last == nodes.len() {
        ui.add_space(4.0);
        ui.small("End of the archived history.");
    }
}
