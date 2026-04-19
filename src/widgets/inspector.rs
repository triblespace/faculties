//! Unified pile inspector — composes all four faculty widgets with a
//! single shared pile-path selector at the top.
//!
//! This is the widget most users actually want: a one-shot viewer for a
//! triblespace pile that surfaces timeline + wiki + compass + messages
//! in a single section, with one path control driving all of them.
//!
//! ```ignore
//! let mut inspector = PileInspector::new("./self.pile");
//! // Inside a GORBIE notebook card:
//! inspector.render(ctx);
//! ```
//!
//! When the user edits the path and clicks "Open", the pile is reopened
//! via a shared [`SharedPile`](crate::widgets::SharedPile) handle and
//! every child widget is re-pointed at the new handle; their internal
//! state (cached workspace, fact space, selected fragment, scroll
//! position, etc.) resets so the next render pulls fresh branches from
//! the new pile. The pile itself is opened exactly once per path change
//! and shared between all four children via cheap `Arc`-based clones.

use std::path::PathBuf;

use GORBIE::prelude::CardCtx;

use crate::widgets::{
    BranchTimeline, CompassBoard, MessagesPanel, SharedPile, WikiViewer,
    timeline::TimelineSource,
};

/// One-stop pile viewer. Owns the four child widgets and a single
/// pile-path text field that drives all of them.
pub struct PileInspector {
    pile_path: PathBuf,
    /// Editable form of the pile path. Committed to the child widgets
    /// when the user clicks "Open".
    pile_path_text: String,
    /// The currently-open pile, shared with every child widget. `None`
    /// when the last open attempt failed (see [`Self::error`]).
    shared_pile: Option<SharedPile>,
    /// Error message from the last failed open attempt, surfaced at the
    /// top of the inspector.
    error: Option<String>,
    timeline: BranchTimeline,
    wiki: WikiViewer,
    compass: CompassBoard,
    messages: MessagesPanel,
}

impl PileInspector {
    /// Build an inspector pointing at a pile on disk. The default branch
    /// names are `compass` / `local-messages` / `wiki`; the timeline
    /// overlays all three.
    pub fn new(pile_path: impl Into<PathBuf>) -> Self {
        let pile_path = pile_path.into();
        let pile_path_text = pile_path.to_string_lossy().into_owned();

        let sources = vec![
            TimelineSource::Compass {
                branch: "compass".to_string(),
            },
            TimelineSource::LocalMessages {
                branch: "local-messages".to_string(),
            },
            TimelineSource::Wiki {
                branch: "wiki".to_string(),
            },
        ];

        // Open the shared pile up front so every child widget starts out
        // pointing at the same underlying handle. If the open fails we
        // fall back to path-only construction and record the error; the
        // child widgets will also try (and fail) to open the path, but
        // the top-level banner tells the user what went wrong.
        match SharedPile::open(&pile_path) {
            Ok(pile) => {
                let timeline =
                    BranchTimeline::multi_with_shared(pile.clone(), sources);
                let wiki = WikiViewer::with_shared(pile.clone());
                let compass = CompassBoard::with_shared(pile.clone(), "compass");
                let messages = MessagesPanel::with_shared(pile.clone(), "local-messages");
                Self {
                    pile_path,
                    pile_path_text,
                    shared_pile: Some(pile),
                    error: None,
                    timeline,
                    wiki,
                    compass,
                    messages,
                }
            }
            Err(e) => {
                let timeline = BranchTimeline::multi(pile_path.clone(), sources);
                let wiki = WikiViewer::new(pile_path.clone());
                let compass = CompassBoard::new(pile_path.clone(), "compass");
                let messages = MessagesPanel::new(pile_path.clone(), "local-messages");
                Self {
                    pile_path,
                    pile_path_text,
                    shared_pile: None,
                    error: Some(e),
                    timeline,
                    wiki,
                    compass,
                    messages,
                }
            }
        }
    }

    /// Retarget every child widget at a new pile path. Called
    /// automatically when the user clicks "Open" on the top bar, but
    /// exposed so host notebooks can drive it programmatically too.
    pub fn set_pile_path(&mut self, path: impl Into<PathBuf>) {
        let path = path.into();
        if path == self.pile_path {
            return;
        }
        self.pile_path = path.clone();
        self.pile_path_text = self.pile_path.to_string_lossy().into_owned();

        match SharedPile::open(&self.pile_path) {
            Ok(pile) => {
                self.timeline.set_shared_pile(pile.clone());
                self.wiki.set_shared_pile(pile.clone());
                self.compass.set_shared_pile(pile.clone());
                self.messages.set_shared_pile(pile.clone());
                self.shared_pile = Some(pile);
                self.error = None;
            }
            Err(e) => {
                // Fall back to path-based re-open on the children so
                // they can retry against the (broken) path and surface
                // their own error banners.
                self.timeline.set_pile_path(&self.pile_path);
                self.wiki.set_pile_path(&self.pile_path);
                self.compass.set_pile_path(&self.pile_path);
                self.messages.set_pile_path(&self.pile_path);
                self.shared_pile = None;
                self.error = Some(e);
            }
        }
    }

    /// Render the inspector into a GORBIE card context.
    pub fn render(&mut self, ctx: &mut CardCtx<'_>) {
        // Top bar: single shared pile-path selector.
        let mut reopen = false;
        ctx.grid(|g| {
            g.place(10, |ctx| {
                ctx.text_field(&mut self.pile_path_text);
            });
            g.place(2, |ctx| {
                if ctx.button("Open").clicked() {
                    reopen = true;
                }
            });
        });
        if reopen {
            let trimmed = self.pile_path_text.trim().to_string();
            self.set_pile_path(PathBuf::from(trimmed));
        }

        // Pile-level error banner (shown above the child sections).
        if let Some(err) = &self.error {
            let color = ctx.ctx().global_style().visuals.error_fg_color;
            ctx.label(
                egui::RichText::new(format!("pile open error: {err}"))
                    .color(color)
                    .monospace()
                    .small(),
            );
        }

        // Each child widget renders its own section.
        self.timeline.render(ctx);
        self.wiki.render(ctx);
        self.compass.render(ctx);
        self.messages.render(ctx);
    }
}
