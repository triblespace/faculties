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
//! When the user edits the path and clicks "Open", every child widget
//! is re-pointed via its `set_pile_path` method; their internal state
//! (pile handle, cached fact space, selected fragment, scroll position,
//! etc.) resets so the next render opens the new pile from scratch.

use std::path::PathBuf;

use GORBIE::prelude::CardCtx;

use crate::widgets::{
    BranchTimeline, CompassBoard, MessagesPanel, WikiViewer,
    timeline::TimelineSource,
};

/// One-stop pile viewer. Owns the four child widgets and a single
/// pile-path text field that drives all of them.
pub struct PileInspector {
    pile_path: PathBuf,
    /// Editable form of the pile path. Committed to the child widgets
    /// when the user clicks "Open".
    pile_path_text: String,
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
        let timeline = BranchTimeline::multi(
            pile_path.clone(),
            vec![
                TimelineSource::Compass {
                    branch: "compass".to_string(),
                },
                TimelineSource::LocalMessages {
                    branch: "local-messages".to_string(),
                },
                TimelineSource::Wiki {
                    branch: "wiki".to_string(),
                },
            ],
        );
        let wiki = WikiViewer::new(pile_path.clone());
        let compass = CompassBoard::new(pile_path.clone(), "compass");
        let messages = MessagesPanel::new(pile_path.clone(), "local-messages");
        Self {
            pile_path,
            pile_path_text,
            timeline,
            wiki,
            compass,
            messages,
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
        self.timeline.set_pile_path(&self.pile_path);
        self.wiki.set_pile_path(&self.pile_path);
        self.compass.set_pile_path(&self.pile_path);
        self.messages.set_pile_path(&self.pile_path);
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

        // Each child widget renders its own section.
        self.timeline.render(ctx);
        self.wiki.render(ctx);
        self.compass.render(ctx);
        self.messages.render(ctx);
    }
}
