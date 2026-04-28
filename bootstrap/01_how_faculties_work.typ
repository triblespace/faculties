= How Faculties Work

Faculties are self-contained `rust-script` scripts living in a
`faculties/` checkout (the canonical upstream is
[github.com/triblespace/faculties](https://github.com/triblespace/faculties)).
Each one is a CLI that reads and writes a TribleSpace pile via
the `triblespace` crate.

== Mental model

  - One faculty = one cognitive verb (`compass.rs` for goals,
    `wiki.rs` for fragments, `files.rs` for archived artefacts,
    `local_messages.rs` for direct messages, etc.)
  - Each faculty owns a *branch* in the pile (`compass`,
    `wiki`, `files`, …) and writes its own commits there.
  - Branches are merged independently; touching `compass` doesn't
    invalidate `wiki`.
  - All scripts honour `PILE=/path/to/self.pile` as an environment
    variable — set it once per session and skip `--pile` on every
    call.

== Discovery

`ls $(dirname $(which wiki.rs))` (or `ls path/to/faculties/`
if you haven't put them on PATH yet) shows what's available.
Each faculty supports `--help` listing its subcommands;
subcommands take their own `--help` for argument detail.

== Why this shape

The agent acts through shell commands and observes concrete
output. A faculty is the smallest possible "verb you can run from
a shell that produces a durable side effect." The pile is the
single source of truth — everything the agent thinks, decides, or
produces accretes there as content-addressed blobs.

This is the *shell-first causality* design: model speaks to the
world via shell, the world speaks back via stdout, and the pile
remembers everything between turns.
