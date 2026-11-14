//! One shared composition for the dashboard, focused CLI wrappers and MCP.

use super::{Target, Viewer};
use crate::widgets::{
    AtlasViewer, BranchTimeline, CompassBoard, DecidePanel, DiscordViewer, FilesViewer,
    GaugeViewer, HeadspaceViewer, MailViewer, MemoryViewer, MessagesPanel, PlannerViewer,
    RelationsViewer, SourceKey, StorageState, TeamsViewer, TimelineSource, TriageViewer,
    WidgetContext, WikiViewer,
};
use triblespace::prelude::Id;
use GORBIE::prelude::*;
use GORBIE::state::StateId;

impl Target {
    pub(crate) fn sources(self) -> &'static [SourceKey] {
        match self {
            Self::Dashboard => &SourceKey::ALL,
            Self::Atlas => &[SourceKey::Atlas],
            Self::Discord => &[SourceKey::Discord],
            Self::Files => &[SourceKey::Files],
            Self::Gauge => &[SourceKey::Wiki],
            Self::Headspace => &[SourceKey::Headspace],
            Self::Memory => &[SourceKey::Memory],
            Self::Messages => &[SourceKey::Messages],
            Self::Planner => &[SourceKey::Planner],
            Self::Teams => &[SourceKey::Teams],
            Self::Triage => &[SourceKey::Triage],
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

    compose_hosted(nb, target, Standalone(storage));
}

/// A file action resolved by the host against the exact clicked observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FileAction {
    WikiLink(String),
    Entity(Id),
}

/// Supplies one frozen render boundary and its selection generation. App hosts
/// perform storage observation and file actions on their own worker. This
/// interface never chooses a pile, workspace, identity or service.
pub trait Host: Clone + 'static {
    fn with_context(
        &self,
        ctx: &mut CardCtx<'_>,
        render: impl FnOnce(
            &mut CardCtx<'_>,
            WidgetContext<'_>,
            u64,
            &mut dyn FnMut(FileAction) -> Result<(), String>,
        ),
    );
}

#[derive(Clone, Copy)]
struct Standalone(StateId<StorageState>);

impl Host for Standalone {
    fn with_context(
        &self,
        ctx: &mut CardCtx<'_>,
        render: impl FnOnce(
            &mut CardCtx<'_>,
            WidgetContext<'_>,
            u64,
            &mut dyn FnMut(FileAction) -> Result<(), String>,
        ),
    ) {
        let mut storage = self.0.read_mut(ctx);
        let sources = storage.context();
        let mut open = |action| {
            let files = sources
                .dataset(SourceKey::Files)
                .ok_or_else(|| "Files are unavailable.".to_owned())?;
            match action {
                FileAction::WikiLink(selector) => {
                    WikiViewer::open_file_with(files, &selector, || Ok(()))
                }
                FileAction::Entity(entity) => {
                    FilesViewer::open_entity_with(files, entity, || Ok(()))
                }
            }
        };
        render(ctx, sources, 0, &mut open);
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
            &mut dyn FnMut(FileAction) -> Result<(), String>,
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
    if target == Target::Dashboard || target == Target::Headspace {
        card(
            nb,
            host.clone(),
            "headspace",
            HeadspaceViewer::default,
            move |ctx, panel, sources, _open| {
                let Some(headspace) = sources.dataset(SourceKey::Headspace) else {
                    return;
                };
                let Some(secrets) = sources.secrets() else {
                    return;
                };
                panel.render(ctx, headspace, secrets);
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
                    TimelineSource::Compass {
                        key: SourceKey::Compass,
                        label: "goals".to_owned(),
                    },
                    TimelineSource::LocalMessages {
                        key: SourceKey::Messages,
                        label: "local".to_owned(),
                    },
                    TimelineSource::Wiki {
                        key: SourceKey::Wiki,
                        label: "wiki".to_owned(),
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

    if target == Target::Dashboard || target == Target::Gauge {
        card(
            nb,
            host.clone(),
            "gauge",
            GaugeViewer::default,
            move |ctx, panel, sources, _open| {
                let Some(view) = sources.dataset(SourceKey::Wiki) else {
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
            "wiki",
            WikiViewer::default,
            move |ctx, wiki, sources, open| {
                let Some(view) = sources.dataset(SourceKey::Wiki) else {
                    return;
                };
                wiki.render_with_file_action(
                    ctx,
                    view,
                    sources.dataset(SourceKey::Files),
                    |selector| open(FileAction::WikiLink(selector.to_owned())),
                );
            },
        );
    }

    if target == Target::Dashboard {
        card(
            nb,
            host.clone(),
            "compass",
            CompassBoard::default,
            move |ctx, compass, sources, _open| {
                let Some(view) = sources.dataset(SourceKey::Compass) else {
                    return;
                };
                compass.render(ctx, view);
            },
        );
    }

    if target == Target::Dashboard {
        card(
            nb,
            host.clone(),
            "decide",
            DecidePanel::default,
            move |ctx, panel, sources, _open| {
                let Some(view) = sources.dataset(SourceKey::Decide) else {
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

    if target == Target::Dashboard || target == Target::Planner {
        card(
            nb,
            host.clone(),
            "planner",
            PlannerViewer::default,
            move |ctx, panel, sources, _open| {
                let Some(view) = sources.dataset(SourceKey::Planner) else {
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
                panel.render_with_file_action(ctx, view, |entity| open(FileAction::Entity(entity)));
            },
        );
    }

    if target == Target::Dashboard || target == Target::Triage {
        card(
            nb,
            host.clone(),
            "triage",
            TriageViewer::default,
            move |ctx, panel, sources, _open| {
                let Some(cognition) = sources.dataset(SourceKey::Triage) else {
                    return;
                };
                let Some(headspace) = sources.dataset(SourceKey::Headspace) else {
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
                panel.render(ctx, cognition, headspace, secrets, relations, messages);
            },
        );
    }

    if target == Target::Dashboard || target == Target::Atlas {
        card(
            nb,
            host.clone(),
            "atlas",
            AtlasViewer::default,
            move |ctx, panel, sources, _open| {
                let Some(view) = sources.dataset(SourceKey::Atlas) else {
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

    #[test]
    fn focused_inputs_keep_the_original_roots_and_storage_owns_dependencies() {
        assert_eq!(Target::Dashboard.sources(), &SourceKey::ALL);
        for (target, source) in [
            (Target::Atlas, SourceKey::Atlas),
            (Target::Discord, SourceKey::Discord),
            (Target::Files, SourceKey::Files),
            (Target::Gauge, SourceKey::Wiki),
            (Target::Headspace, SourceKey::Headspace),
            (Target::Memory, SourceKey::Memory),
            (Target::Messages, SourceKey::Messages),
            (Target::Planner, SourceKey::Planner),
            (Target::Teams, SourceKey::Teams),
            (Target::Triage, SourceKey::Triage),
        ] {
            assert_eq!(target.sources(), &[source]);
        }
    }
}
