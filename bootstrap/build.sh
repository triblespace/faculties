#!/usr/bin/env bash
#
# Regenerate ../bootstrap.pile from the .typ sources in this directory.
# Use this when fragment content drifts; new agents getting onboarded
# pick up the refreshed substrate via `cat bootstrap.pile >> self.pile`.
#
# Usage:
#   cd faculties/bootstrap && ./build.sh
#
set -euo pipefail

BOOTSTRAP_DIR="$(cd "$(dirname "$0")" && pwd)"
PILE_PATH="$BOOTSTRAP_DIR/../bootstrap.pile"
WIKI="$BOOTSTRAP_DIR/../wiki.rs"
COMPASS="$BOOTSTRAP_DIR/../compass.rs"

# Start fresh — old fragments from previous builds would orphan
# but never disappear (piles are append-only). Regenerating into
# a clean file is simpler than de-duping.
rm -f "$PILE_PATH"
touch "$PILE_PATH"
export PILE="$PILE_PATH"

echo "==> Building wiki fragments"

# Each fragment: (title, source-file, tags...). Tags repeat
# `bootstrap onboarding` plus a topic-specific tag.

# 1. How faculties work
"$WIKI" create "How Faculties Work" \
  "@$BOOTSTRAP_DIR/01_how_faculties_work.typ" \
  --tag bootstrap --tag onboarding --tag faculties >/dev/null

# 2. Wiki style guide
"$WIKI" create "Wiki Fragment Style Guide" \
  "@$BOOTSTRAP_DIR/02_wiki_style_guide.typ" \
  --tag bootstrap --tag onboarding --tag wiki >/dev/null

# 3. Compass workflow
"$WIKI" create "Compass Goals Workflow" \
  "@$BOOTSTRAP_DIR/03_compass_workflow.typ" \
  --tag bootstrap --tag onboarding --tag compass >/dev/null

# 4. When to use codex
"$WIKI" create "When to Use Codex (and When Not To)" \
  "@$BOOTSTRAP_DIR/04_when_to_use_codex.typ" \
  --tag bootstrap --tag onboarding --tag codex --tag subagents >/dev/null

# 5. Work as its own ledger
"$WIKI" create "Work As Its Own Ledger" \
  "@$BOOTSTRAP_DIR/05_work_as_its_own_ledger.typ" \
  --tag bootstrap --tag onboarding --tag design --tag principle >/dev/null

# 6. Tool selection
"$WIKI" create "Tool Selection: Faculties First" \
  "@$BOOTSTRAP_DIR/06_tool_selection.typ" \
  --tag bootstrap --tag onboarding --tag tools --tag reference >/dev/null

# 7. Getting started — references siblings by title, not by
# fragment id, so `wiki.rs list --tag bootstrap` is the
# navigation surface. Avoids hardcoded ids that would drift on
# every rebuild (the wiki faculty mints fresh fragment ids per
# build).
"$WIKI" create "Getting Started: Your First Hour" \
  "@$BOOTSTRAP_DIR/07_getting_started.typ" \
  --tag bootstrap --tag onboarding --tag start-here >/dev/null

# 8. Files faculty — usage patterns for `files.rs add/fetch/list`,
# why content-addressing matters, citation conventions for wiki
# fragments.
"$WIKI" create "Files Faculty: Archiving and Citing Artefacts" \
  "@$BOOTSTRAP_DIR/08_files_faculty.typ" \
  --tag bootstrap --tag onboarding --tag files >/dev/null

# 9. Teams — `trible team` lifecycle, env-var-driven config,
# `pile net status` diagnostic, when to revoke. The auth-arc
# tool surface, which is its own user-facing chapter in the
# triblespace-rs book.
"$WIKI" create "Teams: Capability-Based Membership" \
  "@$BOOTSTRAP_DIR/09_teams_faculty.typ" \
  --tag bootstrap --tag onboarding --tag teams --tag auth >/dev/null

# 10. Local messages — direct agent-to-agent DM with read
# acknowledgements. Tool Selection table flags this as the
# right choice for transient, addressee-specific notes.
"$WIKI" create "Local Messages: Agent-to-Agent Direct Messaging" \
  "@$BOOTSTRAP_DIR/10_local_messages_faculty.typ" \
  --tag bootstrap --tag onboarding --tag local-messages --tag coordination >/dev/null

# 11. Orient — situation snapshot of recent messages + doing +
# todo. The "where was I?" command. Includes `orient.rs wait`
# for blocking on relevant branch changes (idle-agent
# coordination primitive).
"$WIKI" create "Orient: The Situation-Snapshot Faculty" \
  "@$BOOTSTRAP_DIR/11_orient_faculty.typ" \
  --tag bootstrap --tag onboarding --tag orient --tag coordination >/dev/null

echo "    11 fragments created"

echo "==> Building compass goals"

"$COMPASS" add "Read the start-here wiki fragment" \
  --tag bootstrap --tag onboarding \
  --note "Run \`wiki.rs list --tag bootstrap\` to find the 'Getting Started: Your First Hour' fragment, then \`wiki.rs show <id>\` to read it. This is your orientation tour." >/dev/null

"$COMPASS" add "Mint your first id with \`trible genid\`" \
  --tag bootstrap --tag faculties \
  --note "Stable IDs in TribleSpace are minted, never guessed. Run \`trible genid\` and copy the 32-char hex output. Try minting 3 in a row — they should all be different." >/dev/null

"$COMPASS" add "Create your first wiki fragment" \
  --tag bootstrap --tag wiki \
  --note "Pick something you've learned today. Write a 5-10 line typst body to /tmp/myfrag.typ, then \`wiki.rs create \"My first fragment\" @/tmp/myfrag.typ --tag personal\`. Verify with \`wiki.rs show <id>\`." >/dev/null

"$COMPASS" add "Archive a file with \`files.rs add\`" \
  --tag bootstrap --tag files \
  --note "Pick any local file (not a binary in a git repo). Run \`files.rs add <path>\`. The output \`files:<hash>\` is a content-addressed reference you can cite from wiki fragments. Confirm the hash is stable: re-run on the same file, same hash." >/dev/null

"$COMPASS" add "Run \`wiki.rs lint\` and \`wiki.rs check\`" \
  --tag bootstrap --tag wiki --tag hygiene \
  --note "lint applies markdown→typst transforms and rebuilds the links_to index. check reports orphan fragments, broken links, truncated ids. Run both. Note any warnings — they're the wiki's self-diagnostic surface." >/dev/null

"$COMPASS" add "Mark this goal done and write an outcome note" \
  --tag bootstrap --tag compass \
  --note "When you finish working through the bootstrap goals, move this one to done with \`compass.rs move <id> done\` and add a final note recording what stuck and what you'd improve. The outcome note IS the audit trail." >/dev/null

echo "    6 goals created"

echo
echo "==> Done. bootstrap.pile is at $PILE_PATH"
echo "    $(ls -lh "$PILE_PATH" | awk '{print $5}') / $(wc -c < "$PILE_PATH") bytes"
