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
compass.rs --pile ./self.pile list
wiki.rs --pile ./self.pile search "typst"
orient.rs --pile ./self.pile
```

No compilation step, no framework to set up. Drop the file into any agent's
workspace, put the directory on PATH, and the tool is available.

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
# Direct invocation (files are executable with a rust-script shebang)
./compass.rs --pile ./self.pile list

# Or add the directory to PATH
export PATH="$(pwd):$PATH"
compass.rs --pile ./self.pile list
```

Most faculties accept `--pile` (defaulting to `./self.pile`) and operate on
a named branch of that pile. They're designed to coexist — multiple
faculties on the same pile, each owning its own branch, all rooted in the
same content-addressed blob store.

## Contributing

Faculties are deliberately simple. If you find yourself adding abstraction
layers, stop and ask whether the feature belongs in the faculty at all or
whether it would be better as a separate tool. Each file should stand
alone — you should be able to copy `wiki.rs` into an unrelated project
and have it just work.

## License

Apache-2.0.
