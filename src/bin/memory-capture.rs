//! Minimal capture target for iterating on the memory widget.
//! Memory chunks live on the `cognition` branch by default (per the
//! memory faculty's DEFAULT_MEMORY_BRANCH, which is "memory" — but
//! older agent runs may have written them to `cognition`).

use std::path::PathBuf;

use faculties::widgets::{MemoryViewer, StorageState};
use GORBIE::notebook;
use GORBIE::prelude::*;

fn resolve_pile_path() -> PathBuf {
    std::env::var("PILE")
        .ok()
        .or_else(|| {
            std::env::args()
                .skip(1)
                .find(|a| !a.starts_with("--"))
        })
        .unwrap_or_else(|| "./self.pile".to_owned())
        .into()
}

#[notebook]
fn main(nb: &mut NotebookCtx) {
    let path = resolve_pile_path();

    let storage = nb.state("storage", StorageState::new(path), |ctx, st| {
        st.top_bar(ctx);
    });

    nb.state("memory", MemoryViewer::default(), move |ctx, panel| {
        let mut st = storage.read_mut(ctx);
        // Try `memory` first, fall back to `cognition` for older
        // agent runs (which wrote chunks to the cognition branch
        // before the memory branch became canonical).
        let ws = st.workspace("memory").or_else(|| st.workspace("cognition"));
        let Some(mut ws) = ws else { return };
        panel.render(ctx, &mut ws);
        st.push(&mut ws);
    });
}
