#!/bin/bash
# Import this machine's new agent sessions into the archive collection.
#
#   ingest-sessions.sh --due                 exit 0 when this host has sessions to import
#   ingest-sessions.sh                       import them, print one receipt line
#   ingest-sessions.sh --since CCYYMMDDhhmm  first run on a host whose older sessions
#                                            are already in the pile
#   ingest-sessions.sh --all                 first run that imports every session here
#
# Each machine imports its own Claude Code transcripts (~/.claude/projects) and
# Codex rollouts (every ~/.codex*/sessions, and $CODEX_HOME/sessions when it is
# set elsewhere; the habit's check may run without the window's CODEX_HOME) with its own
# key, and sync carries the commits to the other piles. Nothing is copied
# between machines: a session present on two of them imports to the same nodes,
# because a node's identity is its content and predecessors, not who imported
# it. The importer only adds what is new in a grown transcript.
#
# It runs as the `ingest-sessions` habit, whose condition is `when @script --due`:
# every window evaluates it on its own machine, so each host is woken for its
# own sessions. --due holds when the last successful run is at least 20 hours
# old (or there is none) and some session file changed since then. Files
# written within the last hour wait for the next run, so a live session is not
# read mid-write; the importer's --finished-after 1h agrees.
#
# The stamp ($XDG_STATE_HOME/faculties/ingest-sessions.stamp) takes the START time
# of a run that finished without error, so a file written during a run is seen
# by the next one. A failed run leaves the stamp alone and exits non-zero.
# Without a stamp the script refuses to run until told where to begin: on a host
# whose sessions were already imported from a copy, re-reading all of them would
# only cost hours, and that is a decision, not a default.
#
# On a GB10 it takes the box lock (mary/scripts/gb10-lock.sh beside this
# checkout, or $GB10_LOCK) and exits 3 when
# the box is busy; the habit then stays due. It needs PILE and the window's
# usual collection environment, as every faculty call does.
set -uo pipefail

STATE=${XDG_STATE_HOME:-$HOME/.local/state}/faculties
STAMP=$STATE/ingest-sessions.stamp
CLAUDE_ROOT=$HOME/.claude/projects
CODEX_ROOTS=()
for d in "$HOME"/.codex*/sessions ${CODEX_HOME:+"$CODEX_HOME/sessions"}; do
  [ -d "$d" ] || continue
  case " ${CODEX_ROOTS[*]-} " in *" $d "*) ;; *) CODEX_ROOTS+=("$d") ;; esac
done
LOCK=${GB10_LOCK:-$(cd "$(dirname "$0")/../.." 2>/dev/null && pwd)/mary/scripts/gb10-lock.sh}
HOST=$(hostname -s)
CHUNK=300
CHUNK_BYTES=1000000000

# -L: PILE is often a symlink, and the link is not the pile.
size() { if [ "$(uname -s)" = Darwin ]; then stat -L -f %z "$1"; else stat -L -c %s "$1"; fi; }

# Session files changed since the stamp (all of them with --all) and quiet for an hour.
changed() {
  local root=$1 newer=()
  [ -d "$root" ] || return 0
  [ -n "${ALL:-}" ] || newer=(-newer "$STAMP")
  # ${a[@]+"${a[@]}"}: macOS bash 3.2 calls an empty array unbound under set -u.
  find "$root" -type f -name '*.jsonl' ${newer[@]+"${newer[@]}"} ! -mmin -60 -print
}

case "${1:-}" in
  --due)
    if [ -f "$STAMP" ] && [ -z "$(find "$STAMP" -mmin +1200 -print)" ]; then exit 1; fi
    if [ ! -f "$STAMP" ]; then exit 0; fi
    hit=$(changed "$CLAUDE_ROOT" | head -1)
    for d in ${CODEX_ROOTS[@]+"${CODEX_ROOTS[@]}"}; do [ -n "$hit" ] && break; hit=$(changed "$d" | head -1); done
    [ -n "$hit" ]
    exit ;;
  --since)
    [ -n "${2:-}" ] || { echo "ingest-sessions: --since needs CCYYMMDDhhmm" >&2; exit 2; }
    mkdir -p "$STATE" && touch -t "$2" "$STAMP" || exit 2 ;;
  --all) ALL=1 ;;
  "") ;;
  *) echo "ingest-sessions: unknown argument $1" >&2; exit 2 ;;
esac

if [ ! -f "$STAMP" ] && [ -z "${ALL:-}" ]; then
  echo "ingest-sessions: no stamp on $HOST. Pass --since CCYYMMDDhhmm (sessions before it are already in the pile) or --all." >&2
  exit 2
fi
[ -n "${PILE:-}" ] || { echo "ingest-sessions: PILE is not set" >&2; exit 2; }
command -v archive >/dev/null || { echo "ingest-sessions: the archive faculty is not on PATH" >&2; exit 2; }

if [ -x "$LOCK" ]; then
  "$LOCK" take "$HOST" "ingest-sessions-$HOST" >/dev/null || { echo "ingest-sessions: $HOST is busy (box lock held); try later" >&2; exit 3; }
  trap '"$LOCK" release "$HOST" "ingest-sessions-$HOST" >/dev/null' EXIT
fi

mkdir -p "$STATE"
START=$STATE/ingest-sessions.start
touch "$START"
t0=$(date +%s)
pile0=$(size "$PILE")
files=0 bytes=0 published=0 unchanged=0
OUT=$(mktemp) ERR=$(mktemp)
trap 'rm -f "$OUT" "$ERR"; [ -x "$LOCK" ] && "$LOCK" release "$HOST" "ingest-sessions-$HOST" >/dev/null' EXIT

# One importer process per chunk of at most CHUNK paths and CHUNK_BYTES bytes.
run_chunk() {
  local src=$1; shift
  [ $# -gt 0 ] || return 0
  archive --pile "$PILE" import --source "$src" --finished-after 1h "$@" > "$OUT" 2> "$ERR"
  local rc=$?
  published=$(( published + $(grep -c 'COMMIT published' "$OUT") ))
  unchanged=$(( unchanged + $(grep -c 'unchanged' "$OUT") ))
  if [ $rc -ne 0 ]; then
    echo "ingest-sessions: $src import failed (rc $rc) at: $(grep -E '^\[[0-9]+/[0-9]+\] ' "$ERR" | tail -1)" >&2
    tail -5 "$ERR" >&2
    exit $rc
  fi
}

PAIRS=("claude-code:$CLAUDE_ROOT")
for d in ${CODEX_ROOTS[@]+"${CODEX_ROOTS[@]}"}; do PAIRS+=("codex:$d"); done
for pair in "${PAIRS[@]}"; do
  src=${pair%%:*} root=${pair#*:}
  chunk=() acc=0
  while IFS= read -r f; do
    sz=$(size "$f")
    if [ ${#chunk[@]} -gt 0 ] && { [ ${#chunk[@]} -ge $CHUNK ] || [ $(( acc + sz )) -gt $CHUNK_BYTES ]; }; then
      run_chunk "$src" ${chunk[@]+"${chunk[@]}"}; chunk=() acc=0
    fi
    chunk+=("$f"); acc=$(( acc + sz )); files=$(( files + 1 )); bytes=$(( bytes + sz ))
  done < <(changed "$root" | sort)
  run_chunk "$src" ${chunk[@]+"${chunk[@]}"}
done

mv "$START" "$STAMP"
echo "ingest-sessions $HOST: $files file(s), $bytes source bytes, $published commit(s) published, $unchanged unchanged, pile +$(( $(size "$PILE") - pile0 )) bytes, $(( $(date +%s) - t0 )) s"
