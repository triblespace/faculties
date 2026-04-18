//! Unified pile inspector: all four faculty widgets in one notebook.
//!
//! Run against a pile that has a `wiki`, `compass`, and `local-messages`
//! branch (plus any branch to feed the timeline):
//!
//! ```ignore
//! cargo run --example pile_inspector --features widgets -- ./self.pile
//! ```
//!
//! Or set `PILE=./self.pile` in the environment.

use std::path::PathBuf;

use faculties::widgets::{BranchTimeline, CompassBoard, MessagesPanel, WikiViewer};
use GORBIE::notebook;
use GORBIE::prelude::*;

fn resolve_pile_path() -> PathBuf {
    std::env::args()
        .nth(1)
        .or_else(|| std::env::var("PILE").ok())
        .unwrap_or_else(|| "./self.pile".to_owned())
        .into()
}

#[notebook]
fn main(nb: &mut NotebookCtx) {
    let pile_path = resolve_pile_path();
    let header = format!("# Pile Inspector\nBrowsing `{}`.", pile_path.display());

    nb.view(move |ctx| {
        ctx.grid(|g| {
            g.full(|ctx| {
                ctx.markdown(&header);
            });
        });
    });

    nb.state(
        "timeline",
        BranchTimeline::new(pile_path.clone(), "compass"),
        |ctx, w| w.render(ctx),
    );

    nb.state(
        "wiki",
        WikiViewer::new(pile_path.clone()),
        |ctx, w| w.render(ctx),
    );

    nb.state(
        "compass",
        CompassBoard::new(pile_path.clone(), "compass"),
        |ctx, w| w.render(ctx),
    );

    nb.state(
        "messages",
        MessagesPanel::new(pile_path, "local-messages"),
        |ctx, w| w.render(ctx),
    );
}
