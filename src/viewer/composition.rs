//! One shared composition for the dashboard, focused CLI wrappers and MCP.

use super::{Target, Viewer};
use crate::goals::Goals;
use crate::storage::Storage;
use crate::widgets::{
    BranchTimeline, ColonyViewer, DecisionsPanel, DiagnosticsViewer, DiscordViewer, EventsViewer,
    FilesViewer, GameWrite, GoalsBoard, KnowledgeHealthViewer, KnowledgeViewer, MailViewer,
    MemoryViewer, MessagesPanel, PriorityGame, ProfilesViewer, RelationsViewer, SchemasViewer,
    SourceKey, StorageState, TeamsViewer, TimelineSource, WidgetContext,
};
use std::sync::Arc;
use triblespace::prelude::Id;
use GORBIE::prelude::*;
use GORBIE::state::StateId;

impl Target {
    pub(crate) fn sources(self) -> &'static [SourceKey] {
        match self {
            Self::Dashboard => &SourceKey::ALL,
            Self::Schemas => &[SourceKey::Schemas],
            Self::Colony => &crate::widgets::colony::INPUTS,
            Self::Discord => &[SourceKey::Discord],
            Self::Files => &[SourceKey::Files],
            Self::KnowledgeHealth => &[SourceKey::Knowledge],
            Self::Profiles => &[SourceKey::Profiles],
            Self::Memory => &[SourceKey::Memory],
            Self::Messages => &[SourceKey::Messages],
            Self::Events => &[SourceKey::Events],
            Self::Teams => &[SourceKey::Teams],
            Self::Diagnostics => &[SourceKey::Diagnostics],
        }
    }
}

/// Add the original ordered cards against the same shared lazy storage state.
/// Arguments are native values; no argv, environment or process exit is read
/// here. Storage retains its frozen observation and dependency closure rules.
pub fn compose(nb: &mut NotebookCtx, viewer: &Viewer, target: Target) {
    let storage = nb.state(
        "storage",
        || StorageState::with_storage(viewer.storage.clone(), target.sources().iter().copied()),
        move |ctx, st| {
            if target == Target::Dashboard {
                ctx.set_default_section_open(false);
            }
            st.top_bar(ctx);
        },
    );

    compose_hosted(
        nb,
        target,
        Standalone {
            storage,
            persona: viewer.persona.as_deref().map(Arc::from),
        },
    );
}

/// An action a widget asks its host to perform. File actions resolve against
/// the exact clicked observation; a priority answer is an ordinary Goals
/// write the host performs with its own storage and identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HostAction {
    KnowledgeLink(String),
    Entity(Id),
    /// Rank `higher` over `lower`, as `Goals::prioritize` does. `Err` is a
    /// refused write (a cycle, or a child ranked under its parent).
    Prioritize {
        higher: Id,
        lower: Id,
    },
    /// Say the goal is unclear to the host's identity, as
    /// `Goals::mark_unclear` does: an attributed note tagged `unclear`
    /// asking for an explanation. It ranks nothing and moves no status.
    MarkGoalUnclear {
        goal: Id,
    },
}

/// Perform one Goals write a widget asked for, with the host's own storage
/// and identity. Every host calls this so they all write the same observation;
/// `None` means the action is not a storage write (opening a file or link),
/// which the host handles itself.
pub fn perform_storage_action(
    storage: &Storage,
    persona: Option<&str>,
    action: &HostAction,
) -> Option<Result<(), String>> {
    let goals = || Goals::with_storage(storage.clone());
    let written = match action {
        HostAction::KnowledgeLink(_) | HostAction::Entity(_) => return None,
        HostAction::Prioritize { higher, lower } => goals()
            .prioritize(&format!("{higher:x}"), &format!("{lower:x}"), persona)
            .map(drop),
        HostAction::MarkGoalUnclear { goal } => goals()
            .mark_unclear(&format!("{goal:x}"), persona)
            .map(drop),
    };
    Some(written.map_err(|error| format!("{error:#}")))
}

/// Supplies one frozen render boundary and its selection generation. App hosts
/// perform storage observation and actions on their own worker. This
/// interface never chooses a pile, workspace, identity or service.
pub trait Host: Clone + 'static {
    fn with_context(
        &self,
        ctx: &mut CardCtx<'_>,
        render: impl FnOnce(
            &mut CardCtx<'_>,
            WidgetContext<'_>,
            u64,
            &mut dyn FnMut(HostAction) -> Result<(), String>,
        ),
    );
}

#[derive(Clone)]
struct Standalone {
    storage: StateId<StorageState>,
    persona: Option<Arc<str>>,
}

impl Host for Standalone {
    fn with_context(
        &self,
        ctx: &mut CardCtx<'_>,
        render: impl FnOnce(
            &mut CardCtx<'_>,
            WidgetContext<'_>,
            u64,
            &mut dyn FnMut(HostAction) -> Result<(), String>,
        ),
    ) {
        let mut state = self.storage.read_mut(ctx);
        let storage = state.storage().clone();
        let sources = state.context();
        let persona = self.persona.as_deref();
        let mut perform = |action| perform(&storage, persona, sources, action);
        render(ctx, sources, 0, &mut perform);
    }
}

/// Perform one action for the standalone host, synchronously. A Goals
/// write moves the pile stamp, so the next render observes it.
fn perform(
    storage: &Storage,
    persona: Option<&str>,
    sources: WidgetContext<'_>,
    action: HostAction,
) -> Result<(), String> {
    let files = || {
        sources
            .dataset(SourceKey::Files)
            .ok_or_else(|| "Files are unavailable.".to_owned())
    };
    if let Some(written) = perform_storage_action(storage, persona, &action) {
        return written;
    }
    match action {
        HostAction::KnowledgeLink(selector) => {
            KnowledgeViewer::open_file_with(files()?, &selector, || Ok(()))
        }
        HostAction::Entity(entity) => FilesViewer::open_entity_with(files()?, entity, || Ok(())),
        HostAction::Prioritize { .. } | HostAction::MarkGoalUnclear { .. } => {
            unreachable!("Goals writes are storage actions")
        }
    }
}

struct Selected<T> {
    generation: Option<u64>,
    value: T,
}

impl<T> Selected<T> {
    fn for_generation(&mut self, generation: u64, init: impl FnOnce() -> T) -> &mut T {
        if self.generation != Some(generation) {
            self.value = init();
            self.generation = Some(generation);
        }
        &mut self.value
    }
}

fn card<H, T, I, F>(nb: &mut NotebookCtx, host: H, key: &str, init: I, mut render: F)
where
    H: Host,
    T: Send + Sync + 'static,
    I: Fn() -> T + Copy + 'static,
    F: FnMut(
            &mut CardCtx<'_>,
            &mut T,
            WidgetContext<'_>,
            &mut dyn FnMut(HostAction) -> Result<(), String>,
        ) + 'static,
{
    nb.state(
        key,
        || Selected {
            generation: None,
            value: init(),
        },
        move |ctx, state| {
            host.with_context(ctx, |ctx, sources, generation, open| {
                render(ctx, state.for_generation(generation, init), sources, open);
            });
        },
    );
}

/// The same ordered cards for standalone notebooks and workspace hosts.
/// The host renders its own selector before these cards.
pub fn compose_hosted(nb: &mut NotebookCtx, target: Target, host: impl Host) {
    if target == Target::Dashboard || target == Target::Profiles {
        card(
            nb,
            host.clone(),
            "profiles",
            ProfilesViewer::default,
            move |ctx, panel, sources, _open| {
                let Some(profiles) = sources.dataset(SourceKey::Profiles) else {
                    return;
                };
                let Some(secrets) = sources.secrets() else {
                    return;
                };
                panel.render(ctx, profiles, secrets);
            },
        );
    }

    if target == Target::Dashboard || target == Target::Colony {
        card(
            nb,
            host.clone(),
            "colony",
            ColonyViewer::default,
            |ctx, panel, sources, _| {
                panel.render(ctx, sources);
            },
        );
    }

    if target == Target::Dashboard {
        card(
            nb,
            host.clone(),
            "timeline",
            || {
                BranchTimeline::multi(vec![
                    TimelineSource::Goals {
                        key: SourceKey::Goals,
                        label: "goals".to_owned(),
                    },
                    TimelineSource::LocalMessages {
                        key: SourceKey::Messages,
                        label: "local".to_owned(),
                    },
                    TimelineSource::Knowledge {
                        key: SourceKey::Knowledge,
                        label: "knowledge".to_owned(),
                    },
                    TimelineSource::Reason {
                        key: SourceKey::Reason,
                        label: "reason".to_owned(),
                    },
                    TimelineSource::Archive {
                        key: SourceKey::Archive,
                        label: "archive".to_owned(),
                    },
                ])
            },
            move |ctx, tl, sources, _open| {
                tl.render(ctx, &sources);
            },
        );
    }

    if target == Target::Dashboard || target == Target::KnowledgeHealth {
        card(
            nb,
            host.clone(),
            "knowledge-health",
            KnowledgeHealthViewer::default,
            move |ctx, panel, sources, _open| {
                let Some(view) = sources.dataset(SourceKey::Knowledge) else {
                    return;
                };
                panel.render(ctx, view);
            },
        );
    }

    if target == Target::Dashboard {
        card(
            nb,
            host.clone(),
            "knowledge",
            KnowledgeViewer::default,
            move |ctx, knowledge, sources, open| {
                let Some(view) = sources.dataset(SourceKey::Knowledge) else {
                    return;
                };
                knowledge.render_with_file_action(
                    ctx,
                    view,
                    sources.dataset(SourceKey::Files),
                    |selector| open(HostAction::KnowledgeLink(selector.to_owned())),
                );
            },
        );
    }

    if target == Target::Dashboard {
        card(
            nb,
            host.clone(),
            "goals",
            GoalsBoard::default,
            move |ctx, goals, sources, _open| {
                let Some(view) = sources.dataset(SourceKey::Goals) else {
                    return;
                };
                goals.render(ctx, view);
            },
        );
    }

    if target == Target::Dashboard {
        card(
            nb,
            host.clone(),
            "priority-game",
            PriorityGame::default,
            move |ctx, game, sources, act| {
                let Some(view) = sources.dataset(SourceKey::Goals) else {
                    return;
                };
                game.render(ctx, view, |answer| {
                    act(match answer {
                        GameWrite::Prioritize { higher, lower } => {
                            HostAction::Prioritize { higher, lower }
                        }
                        GameWrite::Unclear(goal) => HostAction::MarkGoalUnclear { goal },
                    })
                });
            },
        );
    }

    if target == Target::Dashboard {
        card(
            nb,
            host.clone(),
            "decisions",
            DecisionsPanel::default,
            move |ctx, panel, sources, _open| {
                let Some(view) = sources.dataset(SourceKey::Decisions) else {
                    return;
                };
                panel.render(ctx, view);
            },
        );
    }

    if target == Target::Dashboard {
        card(
            nb,
            host.clone(),
            "mail",
            MailViewer::default,
            move |ctx, panel, sources, _open| {
                let Some(view) = sources.dataset(SourceKey::Mail) else {
                    return;
                };
                panel.render(ctx, view, sources.dataset(SourceKey::Relations));
            },
        );
    }

    if target == Target::Dashboard || target == Target::Events {
        card(
            nb,
            host.clone(),
            "events",
            EventsViewer::default,
            move |ctx, panel, sources, _open| {
                let Some(view) = sources.dataset(SourceKey::Events) else {
                    return;
                };
                panel.render(ctx, view, sources.dataset(SourceKey::Relations));
            },
        );
    }

    if target == Target::Dashboard || target == Target::Messages {
        card(
            nb,
            host.clone(),
            "messages",
            MessagesPanel::default,
            move |ctx, panel, sources, _open| {
                let Some(view) = sources.dataset(SourceKey::Messages) else {
                    return;
                };
                panel.render(ctx, view, sources.dataset(SourceKey::Relations));
            },
        );
    }

    if target == Target::Dashboard || target == Target::Discord {
        card(
            nb,
            host.clone(),
            "discord",
            DiscordViewer::default,
            move |ctx, panel, sources, _open| {
                let Some(view) = sources.dataset(SourceKey::Discord) else {
                    return;
                };
                panel.render(ctx, view);
            },
        );
    }

    if target == Target::Dashboard || target == Target::Teams {
        card(
            nb,
            host.clone(),
            "teams",
            TeamsViewer::default,
            move |ctx, panel, sources, _open| {
                let Some(view) = sources.dataset(SourceKey::Teams) else {
                    return;
                };
                panel.render(ctx, view);
            },
        );
    }

    if target == Target::Dashboard {
        card(
            nb,
            host.clone(),
            "relations",
            RelationsViewer::default,
            move |ctx, panel, sources, _open| {
                let Some(view) = sources.dataset(SourceKey::Relations) else {
                    return;
                };
                panel.render(ctx, view);
            },
        );
    }

    if target == Target::Dashboard || target == Target::Memory {
        card(
            nb,
            host.clone(),
            "memory",
            MemoryViewer::default,
            move |ctx, panel, sources, _open| {
                let Some(view) = sources.dataset(SourceKey::Memory) else {
                    return;
                };
                panel.render(ctx, view);
            },
        );
    }

    if target == Target::Dashboard || target == Target::Files {
        card(
            nb,
            host.clone(),
            "files",
            FilesViewer::default,
            move |ctx, panel, sources, open| {
                let Some(view) = sources.dataset(SourceKey::Files) else {
                    return;
                };
                panel.render_with_file_action(ctx, view, |entity| open(HostAction::Entity(entity)));
            },
        );
    }

    if target == Target::Dashboard || target == Target::Diagnostics {
        card(
            nb,
            host.clone(),
            "diagnostics",
            DiagnosticsViewer::default,
            move |ctx, panel, sources, _open| {
                let Some(cognition) = sources.dataset(SourceKey::Diagnostics) else {
                    return;
                };
                let Some(profiles) = sources.dataset(SourceKey::Profiles) else {
                    return;
                };
                let Some(secrets) = sources.secrets() else {
                    return;
                };
                let Some(relations) = sources.dataset(SourceKey::Relations) else {
                    return;
                };
                let Some(messages) = sources.dataset(SourceKey::Messages) else {
                    return;
                };
                panel.render(ctx, cognition, profiles, secrets, relations, messages);
            },
        );
    }

    if target == Target::Dashboard || target == Target::Schemas {
        card(
            nb,
            host.clone(),
            "schemas",
            SchemasViewer::default,
            move |ctx, panel, sources, _open| {
                let Some(view) = sources.dataset(SourceKey::Schemas) else {
                    return;
                };
                panel.render(ctx, view);
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_generation_resets_navigation_even_when_returning_to_same_workspace() {
        let mut selected = Selected {
            generation: None,
            value: vec![99],
        };
        selected.for_generation(1, Vec::new).push(10);
        assert_eq!(selected.for_generation(1, Vec::new), &[10]);
        assert!(selected.for_generation(2, Vec::new).is_empty());
        selected.value.push(20);
        assert!(selected.for_generation(3, Vec::new).is_empty());
    }

    /// The standalone host performs a priority answer as an ordinary Goals
    /// write; the next observation reads the edge, and a refused write comes
    /// back as the error the game shows.
    #[test]
    fn standalone_prioritize_writes_an_edge_the_next_view_reads() {
        let directory = tempfile::tempdir().unwrap();
        let pile = directory.path().join("game.pile");
        let key = directory.path().join("game.key");
        std::fs::File::create(&pile).unwrap();
        crate::storage::initialize_signer(&pile, Some(&key)).unwrap();
        let storage = Storage::new(pile, Some(key));
        let goals = Goals::with_storage(storage.clone());
        let alpha = goals.add("alpha", Default::default()).unwrap().goal;
        let beta = goals.add("beta", Default::default()).unwrap().goal;
        let parent = format!("{alpha:x}");
        let child = goals
            .add(
                "child",
                crate::goals::AddOptions {
                    parent: Some(&parent),
                    ..Default::default()
                },
            )
            .unwrap()
            .goal;

        let mut state = StorageState::with_storage(storage, [SourceKey::Goals]);
        let view = state.context().dataset(SourceKey::Goals).unwrap();
        assert!(crate::goals::active_priority_edges(view.facts).is_empty());

        perform(
            state.storage(),
            None,
            state.loaded_context(),
            HostAction::Prioritize {
                higher: alpha,
                lower: beta,
            },
        )
        .unwrap();
        let view = state.context().dataset(SourceKey::Goals).unwrap();
        assert_eq!(
            crate::goals::active_priority_edges(view.facts),
            std::collections::BTreeSet::from([(alpha, beta)])
        );

        let refused = perform(
            state.storage(),
            None,
            state.loaded_context(),
            HostAction::Prioritize {
                higher: alpha,
                lower: child,
            },
        )
        .unwrap_err();
        assert!(
            refused.contains("children are implicitly prioritized"),
            "{refused}"
        );
    }

    /// The standalone host writes an unclear answer as an attributed note
    /// the next view reads as unclear, with no priority edge.
    #[test]
    fn standalone_unclear_writes_a_note_the_next_view_reads() {
        let directory = tempfile::tempdir().unwrap();
        let pile = directory.path().join("unclear.pile");
        let key = directory.path().join("unclear.key");
        std::fs::File::create(&pile).unwrap();
        crate::storage::initialize_signer(&pile, Some(&key)).unwrap();
        let storage = Storage::new(pile, Some(key));
        let goal = Goals::with_storage(storage.clone())
            .add("puzzling", Default::default())
            .unwrap()
            .goal;

        let mut state = StorageState::with_storage(storage, [SourceKey::Goals]);
        let view = state.context().dataset(SourceKey::Goals).unwrap();
        assert!(crate::goals::unclear_goals(view.facts).is_empty());
        perform(
            state.storage(),
            None,
            state.loaded_context(),
            HostAction::MarkGoalUnclear { goal },
        )
        .unwrap();
        let view = state.context().dataset(SourceKey::Goals).unwrap();
        assert_eq!(
            crate::goals::unclear_goals(view.facts),
            std::collections::BTreeSet::from([goal])
        );
        assert!(crate::goals::active_priority_edges(view.facts).is_empty());
    }

    #[test]
    fn focused_inputs_keep_the_original_roots_and_storage_owns_dependencies() {
        assert_eq!(Target::Dashboard.sources(), &SourceKey::ALL);
        for (target, source) in [
            (Target::Schemas, SourceKey::Schemas),
            (Target::Discord, SourceKey::Discord),
            (Target::Files, SourceKey::Files),
            (Target::KnowledgeHealth, SourceKey::Knowledge),
            (Target::Profiles, SourceKey::Profiles),
            (Target::Memory, SourceKey::Memory),
            (Target::Messages, SourceKey::Messages),
            (Target::Events, SourceKey::Events),
            (Target::Teams, SourceKey::Teams),
            (Target::Diagnostics, SourceKey::Diagnostics),
        ] {
            assert_eq!(target.sources(), &[source]);
        }
    }
}
