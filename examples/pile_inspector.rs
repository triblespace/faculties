//! Unified pile inspector: composes all four faculty widgets with a
//! single shared pile-path selector via `StorageState`.
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

use faculties::widgets::{
    BranchTimeline, CompassBoard, MessagesPanel, StorageState, TimelineSource, WikiViewer,
};
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
    let path = resolve_pile_path();

    let storage = nb.state("storage", StorageState::new(path), |ctx, st| {
        st.top_bar(ctx);
    });

    nb.state(
        "timeline",
        BranchTimeline::multi(vec![
            TimelineSource::Compass {
                label: "goals".to_owned(),
            },
            TimelineSource::LocalMessages {
                label: "local".to_owned(),
            },
            TimelineSource::Wiki {
                label: "wiki".to_owned(),
            },
        ]),
        move |ctx, tl| {
            let mut st = storage.read_mut(ctx);
            // Ensure every source's workspace is pulled before we render.
            let branch_names: &[&str] = &["compass", "local-messages", "wiki"];
            for name in branch_names {
                let _ = st.ensure_workspace(name);
            }
            // Collect `(name, &mut Workspace)` for every source that
            // has a pulled workspace. Indices must match the order of
            // `sources` passed to `BranchTimeline::multi` above.
            let pulled = st.workspace_many(branch_names);
            let mut slots: Vec<(&str, &mut _)> = Vec::with_capacity(branch_names.len());
            for (name, ws) in branch_names.iter().copied().zip(pulled.into_iter()) {
                if let Some(w) = ws {
                    slots.push((name, w));
                }
            }
            tl.render(ctx, slots.as_mut_slice());
        },
    );

    nb.state("wiki", WikiViewer::default(), move |ctx, wiki| {
        let mut st = storage.read_mut(ctx);
        // Make sure both branches are pulled. `files` may be absent.
        let _ = st.ensure_workspace("wiki");
        let _ = st.ensure_workspace("files");
        let mut pair = st.workspace_many(&["wiki", "files"]);
        // Destructure in a way that gives distinct mutable references.
        let (files_slot, wiki_slot) = {
            let mut it = pair.drain(..);
            let wiki_slot = it.next().flatten();
            let files_slot = it.next().flatten();
            (files_slot, wiki_slot)
        };
        let Some(wiki_ws) = wiki_slot else { return };
        wiki.render(ctx, wiki_ws, files_slot);
        st.push_if_dirty("wiki");
    });

    nb.state("compass", CompassBoard::default(), move |ctx, compass| {
        let mut st = storage.read_mut(ctx);
        let Some(ws) = st.ensure_workspace("compass") else { return };
        compass.render(ctx, ws);
        st.push_if_dirty("compass");
    });

    nb.state("messages", MessagesPanel::default(), move |ctx, panel| {
        let mut st = storage.read_mut(ctx);
        let _ = st.ensure_workspace("local-messages");
        let _ = st.ensure_workspace("relations");
        let mut pair = st.workspace_many(&["local-messages", "relations"]);
        let (relations_slot, msgs_slot) = {
            let mut it = pair.drain(..);
            let msgs_slot = it.next().flatten();
            let relations_slot = it.next().flatten();
            (relations_slot, msgs_slot)
        };
        let Some(ws) = msgs_slot else { return };
        panel.render(ctx, ws, relations_slot);
        st.push_if_dirty("local-messages");
    });
}
