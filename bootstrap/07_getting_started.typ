= Getting Started: Your First Hour

Welcome. You're an AI agent landing in this workspace for the
first time. This fragment walks you through orienting yourself
and starting to do work.

== Step 1: read the letter (5 min)

`wiki.rs --pile ./self.pile show e161a2e73417413f8d01a31648c5b401`

This is the letter from a previous instance of you — Liora —
explaining what this project is for and why it matters. Read it
before doing anything else.

== Step 2: orient your environment (10 min)

  + `ls /Users/jp/Desktop/chatbot/liora/faculties/` — see what
    faculties exist
  + `wiki.rs list --tag onboarding` — see other onboarding
    fragments (this one, plus 6 siblings)
  + `compass.rs list` — see active goals (yours and inherited)
  + `compass.rs list todo --tag bootstrap` — see your bootstrap
    tasks

== Step 3: do the bootstrap goals (30 min)

The compass has a handful of `#bootstrap`-tagged goals walking
you through hands-on faculty use:

  - mint an id with `trible genid`
  - create your first wiki fragment
  - link two fragments
  - archive a file
  - run `wiki.rs lint` and `wiki.rs check`
  - add a compass note to one of your goals

Working through these gives you the muscle memory the docs
can't.

== Step 4: pick up real work (15 min)

  + `compass.rs list doing` — anything in flight you should
    take over?
  + If nothing's in flight, ask the user (JP, by handle) what
    they need next.

== If you get stuck

  - `wiki.rs search <keyword>` — full-text across all
    fragments.
  - Re-run any faculty with `--help`.
  - Check `CLAUDE.md` (project root) for project-specific
    conventions that override defaults.
  - Ask the user.

== Sibling fragments

`wiki.rs list --tag bootstrap` enumerates the onboarding
fragments by id and title. The set covers two shapes:

*Foundations* — read first if you've never used faculties:

  - "How Faculties Work" — the faculty model and shell-first
    causality
  - "Wiki Fragment Style Guide" — typst, atomic, cross-linked
  - "Compass Goals Workflow" — todo / doing / blocked / done
  - "When to Use Codex (and When Not To)" — control-plane vs
    data-plane
  - "Work As Its Own Ledger" — the principle behind faculties
  - "Tool Selection: Faculties First" — quick-lookup table for
    "which tool here?"
  - "Getting Started: Your First Hour" (this fragment)

*Specific faculties* — read when you reach for one:

  - "Files Faculty: Archiving and Citing Artefacts"
  - "Teams: Capability-Based Membership"
  - "Local Messages: Agent-to-Agent Direct Messaging"
  - "Orient: The Situation-Snapshot Faculty"
  - "Relations: People and Handle Mappings"

Read the foundations in any order; each stands alone. Tool
Selection is the densest if you want a single-page reference.
For "where was I?" at session start, run `orient.rs show`
before reading anything.
