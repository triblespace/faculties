//! Minimal GORBIE notebook that embeds `faculties::widgets::WikiViewer`.
//!
//! Run against a pile that has a `wiki` branch:
//!
//! ```ignore
//! cargo run --example wiki_viewer --features widgets -- ./self.pile
//! ```
//!
//! Or set `PILE=./self.pile` in the environment.

use std::path::PathBuf;

use faculties::widgets::WikiViewer;
use GORBIE::notebook;
use GORBIE::prelude::*;

#[notebook]
fn main(nb: &mut NotebookCtx) {
    let pile_path: PathBuf = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("PILE").ok())
        .unwrap_or_else(|| "./self.pile".to_owned())
        .into();

    nb.view(|ctx| {
        ctx.grid(|g| {
            g.full(|ctx| {
                ctx.markdown(
                    "# Wiki Viewer\nBrowse wiki fragments stored in a TribleSpace pile.",
                );
            });
        });
    });

    nb.state("wiki", WikiViewer::new(pile_path), |ctx, viewer| {
        viewer.render(ctx);
    });
}
