# Faculties

An office suite for AI agents.

Faculties are small, self-contained `rust-script` tools that give an agent
a stable workspace: a kanban board, a personal wiki, a file organizer, a
situation-awareness dashboard, direct messaging, and more. They persist
their state in a [TribleSpace](https://github.com/triblespace/triblespace-rs)
pile — typically `./self.pile` — so the agent owns its own history across
sessions.

![faculties-viewer composing activity, wiki, compass, and messages widgets](preview.png)

## Getting started

Install a Rust toolchain (if you don't have one):

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Install the GUI viewer and `rust-script` (for the CLI tools):

```sh
cargo install faculties --features widgets     # gives you `faculties-viewer`
cargo install rust-script                      # needed to run *.rs faculties
```

Clone the repo, put it on PATH, and create an empty pile:

```sh
git clone https://github.com/triblespace/faculties
cd faculties
export PATH="$(pwd):$PATH"
touch ./self.pile
export PILE=./self.pile
```

Add a few things through the CLI faculties, then open the viewer:

```sh
compass.rs add "ship the demo" --status doing
wiki.rs create --title "Hello" --body "First *typst* fragment."
faculties-viewer               # picks up PILE from the environment
```

## Why

LLM agents forget. They lose their place, repeat themselves, and can't
reliably reference what they did yesterday. Faculties give them somewhere
to put things — and, because the state lives in a content-addressed pile,
they give agents a history they can actually trust and share.

The design principle: **work is its own ledger**. Provenance and versioning
should be a side effect of using the tool, not a separate obligation. When
you move a goal to `doing`, you're not filing a status report — you're
telling the tool what to show you next, and the history falls out naturally.

## The faculties

| Faculty | Purpose |
|---|---|
| `compass.rs` | Kanban goal/task board with status, tags, notes, priorities |
| `wiki.rs` | Personal wiki with typst fragments, links, and full-text search |
| `files.rs` | File organizer backed by blob storage and tags |
| `orient.rs` | Situation awareness dashboard — what's happening right now |
| `atlas.rs` | Cross-branch map of the pile's contents |
| `gauge.rs` | Metrics and counters |
| `memory.rs` | Long-term memory: compact history and salient fragments |
| `headspace.rs` | Model/prompt configuration |
| `reason.rs` | Record reasoning steps alongside actions |
| `patience.rs` | Soft timers and pacing |
| `local_messages.rs` | Direct messaging between personas and humans |
| `relations.rs` | People, affinity, contact info |
| `teams.rs` | Microsoft Teams archive and bridge |
| `triage.rs` | Workflow staging for inbound items |
| `archive.rs` | Import external archives (chats, exports) into the pile |
| `web.rs` | Web search and fetch with results recorded |

## Notes on pile & branches

Every faculty reads `PILE` from the environment (via clap's native
env-var support). You can pass `--pile <path>` to override it for a
single call. A pile is an append-only file — `touch new.pile` is
literally the whole seed. Faculties operate on named branches of
the pile and are designed to coexist; multiple faculties on the
same pile each own their own branch, all rooted in the same
content-addressed blob store.

## GORBIE viewer

The installed `faculties-viewer` binary composes all four widgets
(activity timeline, wiki graph, compass kanban, local-messages
thread) against a single pile — see the screenshot above.

From a checkout:

```sh
cargo run --release --features widgets --bin faculties-viewer -- ./self.pile
```

Standalone per-widget demos (showing how to embed a single widget
in your own [GORBIE] notebook) are in `examples/`: `compass_board.rs`,
`wiki_viewer.rs`, `messages_panel.rs`, `branch_timeline.rs`, and
`pile_inspector.rs` (source for the binary above).

[GORBIE]: https://github.com/triblespace/GORBIE

## Contributing

Faculties are deliberately simple. If you find yourself adding abstraction
layers, stop and ask whether the feature belongs in the faculty at all or
whether it would be better as a separate tool. Each file should stand
alone — you should be able to copy `wiki.rs` into an unrelated project
and have it just work.

## License

Apache-2.0.
