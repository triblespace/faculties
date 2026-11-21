#!/bin/bash
# test_gollum_plan.sh [WORKDIR] -- gate gollum-plan against the landed
# `wiki import --plan` CLI, on a synthetic repository and a scratch pile.
#
# Every faculty call runs with the whole colony environment removed
# (TRIBLESPACE_*, FACULTIES_*, PILE, PERSONA) and an absolute --pile/--key
# inside WORKDIR. WIKI, FILES, BOOTSTRAP and TRIBLE name the binaries
# exercised (default: the ones on PATH); their sha256 is printed.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
W=${1:-$(mktemp -d /tmp/gollum-plan-gate.XXXXXX)}
mkdir -p "$W"; [ -z "$(ls -A "$W")" ] || { echo "$W is not empty" >&2; exit 1; }
WIKI=${WIKI:-$(command -v wiki)}; FILES=${FILES:-$(command -v files)}
BOOTSTRAP=${BOOTSTRAP:-$(command -v bootstrap)}; TRIBLE=${TRIBLE:-$(command -v trible)}
clean() { env $(env | grep -o '^\(TRIBLESPACE_[A-Z_]*\|FACULTIES_[A-Z_]*\)' | sed 's/^/-u /' | tr '\n' ' ') -u PERSONA -u PILE "$@"; }
P=$W/pile/test.pile; K=$W/pile/self.key
w() { clean "$WIKI" --pile "$P" --key "$K" "$@"; }
f() { clean "$FILES" --pile "$P" --key "$K" "$@"; }
G=$HERE/gollum-plan
fail() { echo "FAIL: $*" >&2; exit 1; }
ok() { echo "ok: $*"; }

echo "workdir $W"
for b in "$WIKI" "$FILES" "$BOOTSTRAP" "$TRIBLE"; do echo "binary $(readlink -f "$b") $(sha256sum "$(readlink -f "$b")" | cut -c1-64)"; done
"$WIKI" import --help | grep -q -- --plan || fail "$WIKI has no import --plan"

TIP=$("$HERE"/gollum-fixture.sh "$W/repo")
echo "fixture tip $TIP"

# Loss is refused unless accepted, and the report names it.
set +e; "$G" plan "$W/repo" "$W/refused" 2>/dev/null; rc=$?; set -e
[ $rc = 2 ] && [ ! -e "$W/refused/plan.json" ] || fail "lossy plan was not refused (rc=$rc)"
ok "lossy plan refused without --accept-loss"

# A symbolic link alone is a loss: a repository whose only loss is one is refused.
S=$W/symlink-only; git init -q -b master "$S"
printf '# Page\n\nSee [[Alias]].\n' > "$S/Page.md"; printf 'bytes' > "$S/data.bin"; ln -s Page.md "$S/Alias.md"
git -C "$S" add -A; git -C "$S" -c user.name=n -c user.email=n@example.test commit -q -m symlink
set +e; "$G" plan "$S" "$W/symlink-refused" 2>"$W/symlink-refused.err"; rc=$?; set -e
[ $rc = 2 ] && [ ! -e "$W/symlink-refused/plan.json" ] || fail "symlink-only loss was not refused (rc=$rc)"
[ "$(grep -c '^LOSS' "$W/symlink-refused.err")" = 1 ] && grep -q "^LOSS symlink .* Alias.md: symbolic link to 'Page.md'" "$W/symlink-refused.err" \
  || fail "symlink-only refusal does not name exactly Alias.md"
ok "a repository whose only loss is a symlink is refused, naming its commit and path"

# A Latin-1 page and an executable file: each is a loss, reported before any
# import, and the bundle keeps the raw bytes and the mode.
L=$W/latin1-mode; git init -q -b master "$L"
printf '# Gr\xfc\xdfe\n\nSch\xf6ne Gr\xfc\xdfe aus Bremen.\n' > "$L/Willkommen.md"
printf '#!/bin/sh\necho hi\n' > "$L/build.sh"; chmod 755 "$L/build.sh"
git -C "$L" add -A; git -C "$L" -c user.name=n -c user.email=n@example.test commit -q -m latin1-and-mode
set +e; "$G" plan "$L" "$W/latin1-refused" 2>"$W/latin1-refused.err"; rc=$?; set -e
[ $rc = 2 ] && [ ! -e "$W/latin1-refused/plan.json" ] || fail "latin1/mode loss was not refused (rc=$rc)"
[ "$(grep -c '^LOSS' "$W/latin1-refused.err")" = 2 ] \
  && grep -q '^LOSS not-utf8 .* Willkommen.md: page is not valid UTF-8 (first bad byte at offset 4)' "$W/latin1-refused.err" \
  && grep -q '^LOSS mode .* build.sh: file mode 100755 not carried' "$W/latin1-refused.err" \
  || fail "latin1/mode refusal does not name exactly Willkommen.md and build.sh"
"$G" bundle "$L" "$W/latin1.bundle" >/dev/null
git clone -q "$W/latin1.bundle" "$W/latin1-clone"
git -C "$W/latin1-clone" cat-file blob master:Willkommen.md | cmp -s - "$L/Willkommen.md" || fail "bundle changed the Latin-1 bytes"
git -C "$W/latin1-clone" ls-tree master -- build.sh | grep -q '^100755 blob ' || fail "bundle lost the 100755 mode"
"$G" plan "$L" "$W/latin1-plan" --accept-loss 2>/dev/null >/dev/null
grep -q $'\xef\xbf\xbd' "$W/latin1-plan/pages/"*.typ || fail "accepted Latin-1 page does not carry the U+FFFD it was reported for"
ok "a Latin-1 page and a 100755 file are each refused as a loss naming the path; the bundle keeps their bytes and mode"

mkdir -p "$W/pile"
clean "$TRIBLE" pile create "$P" >/dev/null
clean "$TRIBLE" pile signing-key init "$P" >/dev/null
clean "$BOOTSTRAP" --pile "$P" --key "$K" import >/dev/null

# The exact bundle goes to Files once; the plan is made from its read-back.
"$G" bundle "$W/repo" "$W/source.bundle" >/dev/null
IMPORT=$(f add "$W/source.bundle" | awk '/^Import:/{print $2}')
BUNDLE=$(f show "$IMPORT" | awk '/^Root:/{print $2}')
[ -n "$BUNDLE" ] || fail "no Files id for the bundle"
f get "$BUNDLE" @- > "$W/back.bundle"
cmp -s "$W/source.bundle" "$W/back.bundle" || fail "bundle read back differs"
git clone -q "$W/back.bundle" "$W/clone"
[ "$(git -C "$W/clone" rev-parse master)" = "$TIP" ] || fail "clone tip differs"
ok "bundle $BUNDLE reads back byte-exact; clone master = $TIP"
C8=$(git -C "$W/repo" log --format=%H --grep="link aliases" master)
for rev in "$C8" "$TIP"; do for path in Links.md uploads/logo.png; do
  [ "$(git -C "$W/clone" ls-tree "$rev" -- "$path")" = "$(git -C "$W/repo" ls-tree "$rev" -- "$path")" ] \
    && git -C "$W/clone" ls-tree "$rev" -- "$path" | grep -q '^120000 blob ' || fail "bundle lost symlink $path at $rev"
done; done
ok "the bundle keeps Links.md and uploads/logo.png as mode 120000 with their targets, at $C8 and at $TIP"

"$G" plan "$W/clone" "$W/plan" --ref master --bundle-id "$BUNDLE" --accept-loss 2>"$W/plan.findings"
"$G" plan "$W/clone" "$W/plan2" --ref master --bundle-id "$BUNDLE" --accept-loss 2>/dev/null >/dev/null
diff -r "$W/plan" "$W/plan2" >/dev/null || fail "plan is not a pure function of the repository"
"$G" plan "$W/repo" "$W/plan3" --ref master --bundle-id "$BUNDLE" --accept-loss 2>/dev/null >/dev/null
diff -r "$W/plan" "$W/plan3" >/dev/null || fail "plan from the clone differs from the original's"
ok "plan deterministic, and the same from the Files read-back as from the original"

w import --plan "$W/plan/plan.json" > "$W/import1.txt"
w import --plan "$W/plan/plan.json" > "$W/import2.txt"
tail -1 "$W/import1.txt"; tail -1 "$W/import2.txt"
grep -q '^14 revisions in plan: 14 new revisions, 14 new provenance records, 3 new files' "$W/import1.txt" || fail "first import counts"
grep -q '^14 revisions in plan: 0 new revisions, 0 new provenance records, 0 new files, 0 publications$' "$W/import2.txt" || fail "replay was not a no-op"
diff <(head -n -1 "$W/import1.txt") <(head -n -1 "$W/import2.txt") >/dev/null || fail "replay minted different ids"
ok "replay publishes nothing and returns the same ids"
id() { awk -v k="$1" '$2 == k {print $1}' "$W/import1.txt"; }
c() { git -C "$W/repo" log --format=%H --grep="$1" master | head -1; }
C1=$(c "Initial wiki"); C2=$(c "step two"); C3=$(c "Rename"); C5=$(c "Add FAQ"); C4=$(git -C "$W/repo" log --format=%H --grep=kitchen side)

# Rename: one entry, three revisions, the new title at the head.
H=$(w history "$(id "$C3:New-Staff-Guide.md")")
echo "$H" | grep -q "v1  $(id "$C1:Onboarding-Guide.md")  Onboarding Guide  parents=\[\]" || fail "rename v1"
echo "$H" | grep -q "v2  $(id "$C2:Onboarding-Guide.md")  Onboarding Guide" || fail "rename v2"
echo "$H" | grep -q "v3  $(id "$C3:New-Staff-Guide.md")  New Staff Guide  parents=\[$(id "$C2:Onboarding-Guide.md")\]  \[head\]" || fail "rename v3"
ok "rename is one entry: Onboarding Guide -> Onboarding Guide -> New Staff Guide"

# Links name native revisions; cycles complete through fixups.
w links | grep -q 'resolve live:        73' && w links | grep -q 'archived target:     0' && w links | grep -q 'never written:       0' || fail "link audit"
D=$(w diff "$(id "$C3:Home.md:fixup")")
echo "$D" | grep -q "^-.*wiki:$(id "$C2:Onboarding-Guide.md")" && echo "$D" | grep -q "^+.*wiki:$(id "$C3:New-Staff-Guide.md")" || fail "Home fixup does not retarget the renamed guide"
D=$(w diff "$(id "$C5:FAQ.md:fixup")")
echo "$D" | grep -q "^-.*wiki:$(id "$C3:Glossary.md")" && echo "$D" | grep -q "^+.*wiki:$(id "$C5:Glossary.md")" || fail "FAQ fixup"
w show "$(id "$C5:Glossary.md")" | grep -q "wiki:$(id "$C5:FAQ.md")" || fail "Glossary does not link FAQ"
w show "$(id "$C1:Home.md:fixup")" >/dev/null
ok "links resolve to native revisions; 0 broken; cycle fixups retarget exactly the cyclic links"

# Attachments: both versions byte-exact, each page pinned to its commit's.
V1=$(git -C "$W/repo" rev-parse "$C1:uploads/floor.png"); V2=$(git -C "$W/repo" rev-parse "$C2:uploads/floor.png")
for v in $V1 $V2; do git -C "$W/repo" cat-file blob $v > "$W/git-$v"; done
H1=$(w diff "$(id "$C4:Office-Plan.md")" | sed -n 's/^-.*files:\([0-9a-f]*\).*/\1/p')
H2=$(w diff "$(id "$C4:Office-Plan.md")" | sed -n 's/^+.*files:\([0-9a-f]*\).*/\1/p')
f get "$H1" @- | cmp -s - "$W/git-$V1" || fail "floor.png v1 bytes"
f get "$H2" @- | cmp -s - "$W/git-$V2" || fail "floor.png v2 bytes"
[ "$(f imports | grep -c ':uploads/floor.png')" = 2 ] || fail "attachment import occurrences"
[ "$(f imports | grep -c ':uploads/map.png')" = 1 ] || fail "map.png import occurrences"
ok "floor.png v1 ($V1) and v2 ($V2) read back byte-exact; Office Plan pins v1 at $C1 and v2 at $C4"

# Provenance has no CLI reader; its source strings are at least in the pile.
for s in "$C3" "New-Staff-Guide.md" "Clara Curator <clara@example.test>"; do grep -aqF "$s" "$P" || fail "provenance text $s absent"; done
ok "provenance: 14 records counted by the importer; commit, path and author strings present in the pile"

grep -E '^LOSS' "$W/plan.findings"
[ "$(grep -c '^LOSS' "$W/plan.findings")" = 5 ] || fail "expected 5 loss findings"
C9=$(git -C "$W/repo" log --format=%H --grep="logo alias" master)
grep -q "^LOSS symlink ${C8:0:12} Links.md: symbolic link to 'Home.md'" "$W/plan.findings" \
  && grep -q "^LOSS symlink ${C8:0:12} uploads/logo.png: symbolic link to 'floor.png'" "$W/plan.findings" \
  && grep -q "^LOSS symlink ${C9:0:12} uploads/logo.png: symbolic link to 'map.png'" "$W/plan.findings" || fail "symlink findings"
# Even accepted, a symlink is neither a page, an attachment, a link target nor content.
for path in Links.md uploads/logo.png; do grep -qF "\"$path\"" "$W/plan/plan.json" && fail "$path is in plan.json"; done
for rev in "$C8" "$TIP"; do for path in Links.md uploads/logo.png; do
  [ ! -e "$W/plan/blobs/$(git -C "$W/repo" rev-parse "$rev:$path")" ] || fail "symlink target blob of $path written"
done; done
grep -rqF 'plan:uploads/logo.png' "$W/plan/pages" && fail "a page links the symlink"
grep -q 'The Links page and the logo are symbolic links' "$W/plan/pages/"*.typ || fail "links to symlinks are not kept as text"
grep -q 'plan:uploads/map.png' "$W/plan/pages/"*.typ || fail "regular neighbour attachment not linked"
[ "$(f imports | grep -c ':uploads/logo.png')" = 0 ] || fail "uploads/logo.png reached Files as an attachment"
w list > "$W/list.txt"
title() { awk -v t="$1" '{id=$1; $1=""; sub(/^ /, "")} $0 == t {print id}' "$W/list.txt"; }
[ -n "$(title Contacts)" ] || fail "regular neighbour Contacts is not a Wiki page"
[ -z "$(title Links)" ] || fail "Links.md became a Wiki page"
ok "symlinks reported at $C8 and its retarget at $C9; with --accept-loss none is a page, attachment, blob or link target"
grep -q '^LOSS deleted .* Office-Plan.md' "$W/plan.findings" && grep -q '^LOSS unsupported-markup .* Notes.textile' "$W/plan.findings" || fail "loss findings"
w show "$(id "$C4:Office-Plan.md")" >/dev/null
ok "deleted Office-Plan.md and unconverted Notes.textile reported by path; the deleted page's Wiki head survives (by design of the plan format)"
echo "PASS ($W)"
