#!/bin/bash
# What is big and untouched in this machine's home? -- the input to cleaning up
# after ourselves.
#
#   leftovers.sh [--size-gb N] [--age-days N]
#
# WHY THIS EXISTS. On 2026-10-17 both Sparks were 97-99% full. On one of them
# 767 GB were three rehearsal folders for a cutover that had finished two and a
# half weeks earlier: copies of piles kept beside the originals, still there
# because nobody's work included deleting them. Creating a folder is part of
# the work; removing it was nobody's step. This script makes the total visible.
#
# It lists every entry directly under $HOME and $HOME/scratch that is at least
# --size-gb (default 2), and every lane directly under $HOME/lanes whatever its
# size, that holds nothing modified within --age-days (default 7). Lanes count
# by number as well as by bytes: seventy small worktrees are clutter too. Dot-entries directly under $HOME are not considered
# (they hold live state). An entry containing a file named `.keep` is listed as
# kept, with the first line of that file as its reason, and does not count.
# For a git worktree it says whether HEAD is on a remote branch and whether the
# tree is clean, from local refs only (it never fetches).
#
# It never deletes anything. The last line always states how many entries were
# examined, so an empty result cannot hide a broken scan. Exit status: 0 when
# it found something to decide, 1 when it found nothing, 126 when the scan
# itself failed.

set -uo pipefail
SIZE_GB=2
AGE_DAYS=7
while [ $# -gt 0 ]; do
  case "$1" in
    --size-gb) SIZE_GB=$2; shift 2 ;;
    --age-days) AGE_DAYS=$2; shift 2 ;;
    *) echo "leftovers: unknown argument $1" >&2; exit 126 ;;
  esac
done
FIND=/usr/bin/find   # a shell function or alias named find must not answer for it
[ -x "$FIND" ] || { echo "leftovers: $FIND is missing" >&2; exit 126; }
[ -d "${HOME:-}" ] || { echo "leftovers: HOME is not a directory" >&2; exit 126; }

examined=0; found=0; kept=0; total=0
report=""
for parent in "$HOME" "$HOME/lanes" "$HOME/scratch"; do
  [ -d "$parent" ] || continue
  for entry in "$parent"/* ; do
    [ -e "$entry" ] || continue
    [ -L "$entry" ] && continue
    case "$entry" in "$HOME/lanes"|"$HOME/scratch") continue ;; esac
    examined=$((examined + 1))
    bytes=$(du -sxb -- "$entry" 2>/dev/null | cut -f1)
    [ -n "$bytes" ] || { echo "leftovers: cannot size $entry" >&2; exit 126; }
    if [ "$parent" != "$HOME/lanes" ]; then
      [ "$bytes" -ge $((SIZE_GB * 1000000000)) ] || continue
    fi
    if [ -n "$("$FIND" "$entry" -xdev -newermt "$AGE_DAYS days ago" -print -quit 2>/dev/null)" ]; then
      continue
    fi
    newest=$("$FIND" "$entry" -xdev -printf '%TY-%Tm-%Td\n' 2>/dev/null | sort | tail -1)
    gb=$(awk -v b="$bytes" 'BEGIN { printf "%.1f", b / 1e9 }')
    if [ -f "$entry/.keep" ]; then
      kept=$((kept + 1))
      report+="kept    ${gb} GB  last change ${newest}  $entry  ($(head -1 "$entry/.keep"))"$'\n'
      continue
    fi
    found=$((found + 1)); total=$((total + bytes))
    # Git state of the entry itself or of the checkouts directly inside it
    # (a lane holds several worktrees).
    git_state=""; dirty=0; unpublished=0; repos=0
    for repo in "$entry" "$entry"/*; do
      [ -e "$repo/.git" ] && [ ! -L "$repo" ] || continue
      repos=$((repos + 1))
      if [ -n "$(git -C "$repo" status --porcelain 2>/dev/null | head -1)" ]; then dirty=$((dirty + 1))
      elif [ -z "$(git -C "$repo" branch -r --contains HEAD 2>/dev/null | head -1)" ]; then unpublished=$((unpublished + 1))
      fi
    done
    [ "$repos" -gt 0 ] && git_state="  git: ${repos} checkout(s), ${dirty} with uncommitted changes, ${unpublished} with HEAD on no remote branch"
    report+="decide  ${gb} GB  last change ${newest}  $entry${git_state}"$'\n'
  done
done
printf '%s' "$report"
echo "leftovers: ${found} to decide ($(awk -v b="$total" 'BEGIN { printf "%.1f", b / 1e9 }') GB), ${kept} kept, ${examined} entries examined (lanes of any size, other entries of at least ${SIZE_GB} GB; nothing changed in ${AGE_DAYS} days)"
[ "$found" -gt 0 ] && exit 0 || exit 1
