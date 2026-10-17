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
# by number as well as by bytes: seventy small worktrees are clutter too.
# Dot-entries directly under $HOME are not considered (they hold live state).
# An entry containing a file named `.keep` is listed as kept, with the first
# line of that file as its reason, and does not count. For git checkouts in an
# entry (the entry itself or its direct children) it reports uncommitted
# changes and HEADs on no remote branch, from local refs only (it never
# fetches).
#
# A second section lists cargo build output anywhere under $HOME: every
# directory holding a CACHEDIR.TAG written by cargo and a .rustc_info.json (a
# target directory; cargo's registry cache has the tag but not the file), at
# any depth, that is at least 0.1 GB and has not changed within --age-days.
# The generic cache-directory signature alone is not enough: Hugging Face and
# Python virtual environments write the same first line. Dot-entries directly
# under $HOME are not searched, nor any directory holding a file named
# `.leftovers-skip`: put one in archived snapshots, whose contents are history
# even when they look like build output. Top-level entries
# alone miss it, because a work folder that is touched daily can hold hundreds
# of gigabytes of stale targets (2026-10-17: 595 GB inside one such folder).
# Build output can always be rebuilt; its lines say "clean", not "decide".
#
# The walk is Python (os.walk over lstat, one filesystem, no symlinks
# followed), so it behaves the same on Linux and macOS. An entry that cannot be
# read completely is reported as "error", never as untouched. It never deletes
# anything. The last line always states how many entries were examined.
# Exit status: 0 when something needs a decision, 1 when nothing does, 126 when
# the scan itself failed.

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
command -v python3 >/dev/null || { echo "leftovers: python3 is missing" >&2; exit 126; }
[ -d "${HOME:-}" ] || { echo "leftovers: HOME is not a directory" >&2; exit 126; }

exec python3 - "$HOME" "$SIZE_GB" "$AGE_DAYS" <<'PY'
import os, stat, subprocess, sys, time

home, size_gb, age_days = sys.argv[1], float(sys.argv[2]), float(sys.argv[3])
cutoff = time.time() - age_days * 86400
lanes, scratch = os.path.join(home, "lanes"), os.path.join(home, "scratch")

MARKERS = {".keep", ".leftovers-skip"}
# Every (device, inode) is counted once across the whole report: hard links
# would otherwise make the sum of the rows larger than what deleting them all
# frees (found on 2026-10-17: 776 GB of rows for 692 GB allocated).
counted = set()

def scan(entry):
    """Allocated bytes and newest mtime under entry, on one filesystem, symlinks
    not followed. Raises OSError if any part cannot be read."""
    st = os.lstat(entry)
    dev = st.st_dev
    total, newest = st.st_blocks * 512, 0.0
    if os.path.islink(entry) or not os.path.isdir(entry):
        return total, newest
    def fail(error):
        raise error
    for path, dirs, files in os.walk(entry, onerror=fail):
        keep = []
        for name in dirs + files:
            s = os.lstat(os.path.join(path, name))
            if s.st_dev != dev:
                continue
            if (s.st_dev, s.st_ino) not in counted:
                counted.add((s.st_dev, s.st_ino))
                total += s.st_blocks * 512
            # Writing a marker is a decision about the entry, not use of it,
            # so it must not make a kept entry look recently changed.
            if name not in MARKERS:
                newest = max(newest, s.st_mtime)
            if name in dirs:
                keep.append(name)
        dirs[:] = keep
    return total, newest

def git(repo, *args):
    r = subprocess.run(["git", "-C", repo, *args], capture_output=True, text=True)
    return r.stdout.strip() if r.returncode == 0 else None

def git_state(entry):
    repos = dirty = unpublished = 0
    candidates = [entry]
    if os.path.isdir(entry):
        candidates += [os.path.join(entry, c) for c in sorted(os.listdir(entry))]
    for repo in candidates:
        if os.path.islink(repo) or not os.path.exists(os.path.join(repo, ".git")):
            continue
        repos += 1
        if git(repo, "status", "--porcelain"):
            dirty += 1
        elif not git(repo, "branch", "-r", "--contains", "HEAD"):
            unpublished += 1
    if not repos:
        return ""
    return (f"  git: {repos} checkout(s), {dirty} with uncommitted changes, "
            f"{unpublished} with HEAD on no remote branch")

examined = found = kept = errors = 0
total_bytes = 0
lines = []
for parent in (home, lanes, scratch):
    if not os.path.isdir(parent):
        continue
    try:
        names = sorted(os.listdir(parent))
    except OSError as error:
        # A parent we cannot list is a failed scan, never an empty one.
        print(f"leftovers: cannot list {parent}: {error}", file=sys.stderr)
        sys.exit(126)
    for name in names:
        entry = os.path.join(parent, name)
        if parent == home and name.startswith("."):
            continue
        if entry in (lanes, scratch) or os.path.islink(entry):
            continue
        examined += 1
        try:
            size, newest = scan(entry)
        except OSError as error:
            errors += 1
            lines.append(f"error   {entry}  (could not read all of it: {error})")
            continue
        if parent != lanes and size < size_gb * 1e9:
            continue
        if newest >= cutoff:
            continue
        stamp = time.strftime("%Y-%m-%d", time.localtime(newest))
        keep = os.path.join(entry, ".keep")
        try:
            keep_is_file = stat.S_ISREG(os.lstat(keep).st_mode)
        except OSError:
            keep_is_file = False
        if keep_is_file:
            # A regular file only, opened without following links, like the walk.
            kept += 1
            fd = os.open(keep, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
            with os.fdopen(fd, errors="replace") as f:
                reason = f.readline().strip()
            lines.append(f"kept    {size / 1e9:.1f} GB  last change {stamp}  {entry}  ({reason})")
            continue
        found += 1
        total_bytes += size
        lines.append(f"decide  {size / 1e9:.1f} GB  last change {stamp}  {entry}{git_state(entry)}")
# Cargo build output at any depth, found by its CACHEDIR.TAG signature.
CARGO_TAG = b"Signature: 8a477f597d28d172789f06886806bc55"
home_dev = os.lstat(home).st_dev
targets = []
def note(error):
    global errors
    errors += 1
    lines.append(f"error   {getattr(error, 'filename', '?')}  (could not search it: {error})")
for path, dirs, files in os.walk(home, onerror=note):
    if "CACHEDIR.TAG" in files and ".rustc_info.json" in files:
        try:
            with open(os.path.join(path, "CACHEDIR.TAG"), "rb") as f:
                head = f.read(256)
            if head.startswith(CARGO_TAG) and b"created by cargo" in head:
                targets.append(path)
                dirs[:] = []
                continue
        except OSError:
            pass
    keep = []
    for d in dirs:
        if path == home and d.startswith("."):
            continue
        if os.path.exists(os.path.join(path, d, ".leftovers-skip")):
            continue
        full = os.path.join(path, d)
        try:
            s = os.lstat(full)
        except OSError:
            continue
        if d != ".git" and stat.S_ISDIR(s.st_mode) and s.st_dev == home_dev:
            keep.append(d)
    dirs[:] = keep
to_clean = clean_bytes = 0
for target in targets:
    try:
        size, newest = scan(target)
    except OSError as error:
        errors += 1
        lines.append(f"error   {target}  (could not read all of it: {error})")
        continue
    if size < 0.1e9 or newest >= cutoff:
        continue
    to_clean += 1
    clean_bytes += size
    stamp = time.strftime("%Y-%m-%d", time.localtime(newest))
    lines.append(f"clean   {size / 1e9:.1f} GB  last change {stamp}  {target}  (cargo build output)")

for line in lines:
    print(line)
print(f"leftovers: {to_clean} build outputs to clean ({clean_bytes / 1e9:.1f} GB) of {len(targets)} found")
print(f"leftovers: {found} to decide ({total_bytes / 1e9:.1f} GB), {kept} kept, {errors} unreadable, "
      f"{examined} entries examined (lanes of any size, other entries of at least {size_gb:g} GB; "
      f"nothing changed in {age_days:g} days)")
sys.exit(0 if found or errors or to_clean else 1)
PY
