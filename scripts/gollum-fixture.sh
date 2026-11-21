#!/bin/bash
# gollum-fixture.sh DIR -- a synthetic Gollum-style git wiki for gollum-plan's
# tests and its CLI gate. Fixed authors and dates, so every commit sha is the
# same on every run. Covers: edits, a git-mv rename, Gollum and Markdown links,
# link cycles inside one commit, a forward (dangling) link, two versions of an
# image, a branch and a merge, a deletion, an unsupported markup page, and
# symbolic links shaped like a page and like an attachment, added beside a
# regular page and attachment and later retargeted on their own.
set -euo pipefail
D=${1:?fixture directory}
[ ! -e "$D" ] || { echo "$D exists" >&2; exit 1; }
git init -q -b master "$D"
cd "$D"
git config user.name nobody; git config user.email nobody@example.test
commit() { # NAME EMAIL DATE MESSAGE
  GIT_AUTHOR_NAME="$1" GIT_AUTHOR_EMAIL="$2" GIT_AUTHOR_DATE="$3" \
  GIT_COMMITTER_NAME="$1" GIT_COMMITTER_EMAIL="$2" GIT_COMMITTER_DATE="$3" \
    git commit -q -m "$4"
}
png() { # A tiny valid PNG whose pixel is the given byte.
  python3 -c 'import sys,zlib,struct
def chunk(t,d): return struct.pack(">I",len(d))+t+d+struct.pack(">I",zlib.crc32(t+d)&0xffffffff)
sys.stdout.buffer.write(b"\x89PNG\r\n\x1a\n"+chunk(b"IHDR",struct.pack(">IIBBBBB",1,1,8,0,0,0,0))+chunk(b"IDAT",zlib.compress(bytes([0,int(sys.argv[1])])))+chunk(b"IEND",b""))' "$1"
}
mkdir -p uploads
cat > Home.md <<'MD'
# Home

Welcome to the **Example** company wiki.

- See [[Onboarding Guide]] for new staff.
- The [office plan](Office-Plan) shows rooms.

Prices: $5 at 50% off; mail <help@example.test>, use snake_case and #3 // not a comment; 1--2 and a*b*c.
Visit https://example.test/docs_page for more.
MD
cat > Onboarding-Guide.md <<'MD'
# Onboarding Guide

Everything a new colleague needs in the first week.

1. Get a laptop from the front desk.
2. Ask for a door badge.
3. Read the security policy.
4. Set up your mail account.
5. Join the Monday stand-up.

Back to [[Home]].
MD
cat > Office-Plan.md <<'MD'
# Office Plan

![Floor plan](uploads/floor.png)

Back to [[Home]].
MD
png 0 > uploads/floor.png
git add -A; commit "Anna Admin" anna@example.test "2019-03-01T10:00:00+01:00" "Initial wiki"

cat > Onboarding-Guide.md <<'MD'
# Onboarding Guide

Everything a new colleague needs in the first week.

1. Get a laptop from the front desk.
2. Ask for a door badge.
3. Read the security policy.
4. Set up your mail account.
5. Join the Monday stand-up.
6. Meet the team.

Back to [[Home]].
MD
png 255 > uploads/floor.png
git add -A; commit "Ben Builder" ben@example.test "2019-04-02T14:30:00+02:00" "Onboarding: add step two; new floor plan"

git mv Onboarding-Guide.md New-Staff-Guide.md
cat > New-Staff-Guide.md <<'MD'
# New Staff Guide

Everything a new colleague needs in the first week.

1. Get a laptop from the front desk.
2. Ask for a door badge.
3. Read the security policy.
4. Set up your mail account.
5. Join the Monday stand-up.
6. Meet the team.

The plan: [[uploads/floor.png|alt=Floor plan]]. Back to [[Home]].
MD
sed -i 's/\[\[Onboarding Guide\]\]/[[New Staff Guide]]/' Home.md
cat > Glossary.md <<'MD'
# Glossary

See [[FAQ]] (written later).

| Term | Meaning |
|------|---------|
| *SSO* | single sign-on |
| `kbd` | a key |

> Quoted wisdom with **bold** text.

```ruby
puts "#not typst"
```
MD
git add -A; commit "Clara Curator" clara@example.test "2020-01-15T09:00:00+01:00" "Rename Onboarding Guide to New Staff Guide; add Glossary"

git checkout -q -b side
cat > Office-Plan.md <<'MD'
# Office Plan

![Floor plan](uploads/floor.png)

Room 2 is the kitchen. Back to [[Home]].
MD
git add -A; commit "Dan Drafter" dan@example.test "2020-02-01T12:00:00+01:00" "Office: kitchen"

git checkout -q master
cat > FAQ.md <<'MD'
# FAQ

Terms are in the [[Glossary]].
MD
sed -i 's/(written later)/(now written)/' Glossary.md
git add -A; commit "Eve Editor" eve@example.test "2020-02-02T12:00:00+01:00" "Add FAQ"

GIT_AUTHOR_NAME="Frank Merger" GIT_AUTHOR_EMAIL=frank@example.test GIT_AUTHOR_DATE="2020-02-03T12:00:00+01:00" \
GIT_COMMITTER_NAME="Frank Merger" GIT_COMMITTER_EMAIL=frank@example.test GIT_COMMITTER_DATE="2020-02-03T12:00:00+01:00" \
  git merge -q --no-ff side -m "Merge side"

git rm -q Office-Plan.md
printf 'h1. Notes\n\nTextile, which gollum-plan does not convert.\n' > Notes.textile
git add -A; commit "Gina Gardener" gina@example.test "2020-03-01T08:00:00+01:00" "Retire the office plan; add notes"

ln -s Home.md Links.md
ln -s floor.png uploads/logo.png
cat > Contacts.md <<'MD'
# Contacts

Reception is on the [map](uploads/map.png). Back to [[Home]].

The [[Links]] page and the ![logo](uploads/logo.png) are symbolic links.
MD
png 7 > uploads/map.png
git add -A; commit "Hana Helper" hana@example.test "2020-04-01T08:00:00+02:00" "Add contacts and map; link aliases"

ln -sfn map.png uploads/logo.png
git add -A; commit "Ivo Indexer" ivo@example.test "2020-04-02T08:00:00+02:00" "Point the logo alias at the map"
git rev-parse HEAD
