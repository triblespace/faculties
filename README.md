# Faculties

An office suite for AI agents.

Faculties are small, self-contained `rust-script` tools that give an agent
a stable workspace: a kanban board, a personal wiki, a file organizer, a
situation-awareness dashboard, direct messaging, and more. They persist
their state in a [TribleSpace](https://github.com/triblespace/triblespace-rs)
pile — typically `./self.pile` — so the agent owns its own history across
sessions.

Each faculty is a single `.rs` file you can run directly:

```sh
export PILE=./self.pile        # set once per shell
compass.rs list
wiki.rs search "typst"
orient.rs show
```

No compilation step, no framework to set up. Drop the files into any
agent's workspace, put the directory on `PATH`, set `PILE`, and the
tools are available. Every faculty honors the `PILE` environment
variable — you can still pass `--pile <path>` explicitly if you need
to operate on a different pile for a single call.

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

## Requirements

- [`rust-script`](https://rust-script.org/) on PATH
- Network access for the first run of each faculty (dependency fetch + compile)

## Using

```sh
# Point all faculties at a pile once per shell session.
export PILE=./self.pile

# Put the faculties on PATH.
export PATH="$(pwd):$PATH"

# Now invoke faculties directly — no --pile ceremony on every call.
compass.rs list
wiki.rs search "typst"
orient.rs show
```

Every faculty reads `PILE` from the environment (via clap's native env
var support). You can still pass `--pile <path>` explicitly to override
the env var for a single call — useful when you want to operate on a
different pile temporarily.

Faculties operate on named branches of the pile and are designed to
coexist — multiple faculties on the same pile, each owning its own
branch, all rooted in the same content-addressed blob store.

## GORBIE viewer (experimental)

A GUI inspector that renders the same pile branches the CLI faculties
write — compass kanban, wiki graph, local-messages thread, and a
multi-source activity timeline — inside a single [GORBIE] notebook
window.

[GORBIE]: https://github.com/triblespace/GORBIE

```sh
# From the faculties/ checkout:
cargo run --release --example pile_inspector --features widgets -- ./self.pile
```

Standalone per-widget demos are also in `examples/`:
`compass_board.rs`, `wiki_viewer.rs`, `messages_panel.rs`,
`branch_timeline.rs`.

### Creating a demo pile

If you don't already have a `./self.pile`, create one by seeding a
few faculties against a fresh path:

```sh
export PILE=./demo.pile
compass.rs add "ship the demo" --status doing
compass.rs add "write the README" --status done
wiki.rs create --title "Hello" --body "First *typst* fragment."
local_messages.rs send --to <peer-id> "heyyy"
```

Then point the viewer at the new pile:

```sh
cargo run --release --example pile_inspector --features widgets -- ./demo.pile
```

### Building from a checkout

The viewer depends on [GORBIE]. If you're iterating on both, clone
GORBIE as a sibling directory — the repo's `Cargo.toml` has
`[patch.crates-io] GORBIE = { path = "../GORBIE" }` for local
development. Users who just want to build against the published
crate can remove that patch line after cloning.

### Known limitations

- Requires a working `egui 0.34.1`. There is an open upstream panic
  ([emilk/egui#7870]) in `hit_test.rs:365` that can fire when a
  click-sensing and drag-sensing widget overlap. We've routed around
  it in every faculty widget (manual drag on timeline, wiki graph,
  floating cards), but the underlying bug is still latent in any
  other egui app you compose with these. Watch that issue for the
  upstream fix.

[emilk/egui#7870]: https://github.com/emilk/egui/issues/7870

## Contributing

Faculties are deliberately simple. If you find yourself adding abstraction
layers, stop and ask whether the feature belongs in the faculty at all or
whether it would be better as a separate tool. Each file should stand
alone — you should be able to copy `wiki.rs` into an unrelated project
and have it just work.

## License

Apache-2.0.
