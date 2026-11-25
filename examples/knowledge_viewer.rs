//! Minimal GORBIE notebook that embeds `faculties::widgets::KnowledgeViewer`.
//!
//! Run against a pile that has a `knowledge` branch:
//!
//! ```ignore
//! cargo run --example knowledge_viewer --features widgets -- ./self.pile
//! ```
//!
//! Or set `PILE=./self.pile` in the environment.
//!
//! Built with `--features widgets,wemm` (a GB10), the graph also colours every
//! note by its meaning's nearness to a typed sentence: the \u{2248} button
//! beside GO. The model binds on the first such request, on a worker thread.

use std::path::PathBuf;

use faculties::widgets::{KnowledgeViewer, SourceKey, StorageState};
use GORBIE::notebook;
use GORBIE::prelude::*;

#[notebook]
fn main(nb: &mut NotebookCtx) {
    let pile_path: PathBuf = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("PILE").ok())
        .unwrap_or_else(|| "./self.pile".to_owned())
        .into();

    #[cfg(feature = "wemm")]
    let pile_for_semantic = pile_path.clone();

    let storage = nb.state(
        "storage",
        move || StorageState::new(pile_path),
        |ctx, st| {
            st.top_bar(ctx);
        },
    );

    nb.view(|ctx| {
        ctx.grid(|g| {
            g.full(|ctx| {
                ctx.markdown(
                    "# Knowledge Viewer\nBrowse knowledge fragments stored in a TribleSpace pile.",
                );
            });
        });
    });

    #[cfg(feature = "wemm")]
    let semantic = semantic::Worker::spawn(pile_for_semantic);
    #[cfg(feature = "wemm")]
    let init = || KnowledgeViewer::new().with_semantic_colouring();
    #[cfg(not(feature = "wemm"))]
    let init = KnowledgeViewer::default;

    nb.state("knowledge", init, move |ctx, viewer| {
        #[cfg(feature = "wemm")]
        semantic.serve(viewer);
        let mut st = storage.read_mut(ctx);
        let sources = st.context();
        let Some(knowledge) = sources.dataset(SourceKey::Knowledge) else {
            return;
        };
        viewer.render(ctx, knowledge, sources.dataset(SourceKey::Files));
    });
}

/// The host half of semantic colouring: one worker thread that binds the WeMM
/// model on the first request and keeps it, so the UI thread never embeds.
#[cfg(feature = "wemm")]
mod semantic {
    use std::path::PathBuf;
    use std::sync::mpsc::{channel, Receiver, Sender};

    use faculties::knowledge::Knowledge;
    use faculties::widgets::KnowledgeViewer;

    type Answer = (
        String,
        Result<(Vec<(f64, triblespace::core::id::Id)>, Option<String>), String>,
    );

    pub struct Worker {
        requests: Sender<String>,
        answers: Receiver<Answer>,
    }

    impl Worker {
        pub fn spawn(pile: PathBuf) -> Self {
            let (requests, inbox) = channel::<String>();
            let (outbox, answers) = channel::<Answer>();
            std::thread::spawn(move || {
                let mut session = None;
                for query in inbox {
                    let result = (|| {
                        if session.is_none() {
                            session = Some(faculties::wemm::Session::from_env(&pile)?);
                        }
                        Knowledge::new(pile.clone(), None)
                            .similar_scores_with(session.as_ref().unwrap(), &query)
                    })()
                    .map_err(|error| format!("{error:#}"));
                    if outbox.send((query, result)).is_err() {
                        return;
                    }
                }
            });
            Worker { requests, answers }
        }

        /// Pass the viewer's request on, and its answers back.
        pub fn serve(&self, viewer: &mut KnowledgeViewer) {
            if let Some(query) = viewer.take_semantic_request() {
                if self.requests.send(query.clone()).is_err() {
                    viewer.set_semantic_error(&query, "the WeMM worker has stopped".into());
                }
            }
            while let Ok((query, result)) = self.answers.try_recv() {
                match result {
                    Ok((scores, note)) => viewer.set_semantic_scores(
                        &query,
                        scores.into_iter().map(|(cosine, id)| (id, cosine as f32)),
                        note,
                    ),
                    Err(error) => viewer.set_semantic_error(&query, error),
                }
            }
        }
    }
}
