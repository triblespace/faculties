//! Unified pile inspector: `PileInspector` composes all four faculty
//! widgets with a single shared pile-path selector.
//!
//! Run against a pile that has a `wiki`, `compass`, and `local-messages`
//! branch:
//!
//! ```ignore
//! cargo run --example pile_inspector --features widgets -- ./self.pile
//! ```
//!
//! Or set `PILE=./self.pile` in the environment.

use std::path::PathBuf;

use faculties::widgets::PileInspector;
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

    nb.state("inspector", PileInspector::new(pile_path), |ctx, w| {
        w.render(ctx);
    });
}
