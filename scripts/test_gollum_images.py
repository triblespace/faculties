#!/usr/bin/env python3
"""Embedded Gollum images: a fresh import, and the correction of a history an
earlier converter imported with image links, on local temporary data only.

Binaries come from WIKI, FILES, TRIBLE and BOOTSTRAP (default: PATH). OLD_WIKI,
when set, performs the earlier import with a wiki binary from before images
were embedded; otherwise the earlier rendering this converter reproduces is
imported as a plan.
"""
import hashlib
import json
import os
from pathlib import Path
import re
import struct
import subprocess
import sys
import tempfile
import zlib

HERE = Path(__file__).resolve().parent
BASE = "http://wiki.example.invalid/"
# Two synthetic 4x3 JPEGs of one colour each, generated once for this test.
JPEG_V1 = bytes.fromhex(
    "ffd8ffe000104a46494600010100000100010000ffdb004300100b0c0e0c0a100e0d0e1211101318281a181616183123"
    "251d283a333d3c3933383740485c4e404457453738506d51575f626768673e4d71797064785c656763ffdb0043011112"
    "121815182f1a1a2f63423842636363636363636363636363636363636363636363636363636363636363636363636363"
    "6363636363636363636363636363ffc00011080003000403012200021101031101ffc400150001010000000000000000"
    "0000000000000005ffc40014100100000000000000000000000000000000ffc400150101010000000000000000000000"
    "0000000406ffc40014110100000000000000000000000000000000ffda000c03010002110311003f0096004a67ffd9")
JPEG_V2 = bytes.fromhex(
    "ffd8ffe000104a46494600010100000100010000ffdb004300100b0c0e0c0a100e0d0e1211101318281a181616183123"
    "251d283a333d3c3933383740485c4e404457453738506d51575f626768673e4d71797064785c656763ffdb0043011112"
    "121815182f1a1a2f63423842636363636363636363636363636363636363636363636363636363636363636363636363"
    "6363636363636363636363636363ffc00011080003000403012200021101031101ffc400150001010000000000000000"
    "0000000000000006ffc40014100100000000000000000000000000000000ffc400150101010000000000000000000000"
    "0000000306ffc40014110100000000000000000000000000000000ffda000c03010002110311003f009c014c17ffd9")


def png(rgb):
    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))
    header = struct.pack(">IIBBBBB", 1, 1, 8, 2, 0, 0, 0)
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", header)
            + chunk(b"IDAT", zlib.compress(b"\x00" + bytes(rgb))) + chunk(b"IEND", b""))


DIAGRAM = png((9, 90, 200))

HOME_V1 = f"""# ![Logo](/uploads/diagram.png)

The ![in rack picture]({BASE}uploads/rack-photo.jpeg) sits beside [the diagram](/uploads/diagram.png).

| part | view |
|------|------|
| rack | ![cell](uploads/diagram.png) |

[![button](/uploads/diagram.png)](Other)

![elsewhere](https://elsewhere.example.invalid/x.png) and [the other page]({BASE}Other)
and ![a document](/uploads/notes.pdf) and ![missing]({BASE}uploads/none.png).

[[/uploads/diagram.png|alt=Framed diagram|frame]]
"""

GUIDE = f""":toc:
= Guide

image:/uploads/diagram.png[Inline diagram] in a sentence.

.Rack from the front
image::uploads/rack-photo.jpeg[Rack]

image::{BASE}uploads/diagram.png[Absolute, title="Titled diagram"]
"""

# Byte-identical earlier renderings: an image and a deliberate link, on lines
# a person later swaps, duplicates and edits.
EDITED = """Photo: ![same](/uploads/diagram.png)

Download: [same](/uploads/diagram.png)

Rack: ![rack](/uploads/rack-photo.jpeg)

Twice: ![twice](/uploads/diagram.png)
"""


def main():
    env = {k: v for k, v in os.environ.items()
           if k not in {"PILE", "PERSONA"} and not k.startswith(("TRIBLESPACE_", "FACULTIES_", "DRIVE_"))}
    wiki = env.get("WIKI", "wiki")
    files = env.get("FILES", "files")
    trible = env.get("TRIBLE", "trible")
    with tempfile.TemporaryDirectory(prefix="gollum images ") as tmp:
        root = Path(tmp)
        repo = root / "studio wiki"

        def run(*args, succeeds=True, cwd=root):
            result = subprocess.run([str(a) for a in args], cwd=cwd, env=env, capture_output=True)
            if (result.returncode == 0) != succeeds:
                raise AssertionError(f"{args} exited {result.returncode}: "
                                     f"{result.stderr.decode(errors='replace')}{result.stdout.decode(errors='replace')}")
            return result.stdout.decode()

        def git(*args):
            return run("git", "-C", repo, "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                       "-c", "commit.gpgSign=false", *args).strip()

        def pile(name):
            directory = root / name
            directory.mkdir()
            run(trible, "pile", "create", directory / "test.pile")
            run(trible, "pile", "signing-key", "init", directory / "test.pile", "--key", directory / "test.key")
            opts = ["--pile", directory / "test.pile", "--key", directory / "test.key"]
            run(env.get("BOOTSTRAP", "bootstrap"), *opts, "import")
            return opts

        run("git", "init", "--quiet", "-b", "main", repo)
        (repo / "uploads").mkdir()
        (repo / "uploads" / "rack-photo.jpeg").write_bytes(JPEG_V1)
        (repo / "uploads" / "diagram.png").write_bytes(DIAGRAM)
        (repo / "uploads" / "notes.pdf").write_bytes(b"%PDF-1.4 synthetic")
        (repo / "Home.md").write_text(HOME_V1)
        (repo / "Other.md").write_text("Another page.\n")
        (repo / "Pinned.md").write_text("![pinned](/uploads/rack-photo.jpeg)\n")
        (repo / "Guide.adoc").write_text(GUIDE)
        (repo / "Edited.md").write_text(EDITED)
        (repo / "Flip.md").write_text("Photo: ![flip](/uploads/diagram.png)\n")
        git("add", "--all")
        git("commit", "--quiet", "-m", "first")
        first = git("rev-parse", "HEAD")
        # New bytes at the same path, and a page that changes with them.
        (repo / "uploads" / "rack-photo.jpeg").write_bytes(JPEG_V2)
        (repo / "Home.md").write_text(HOME_V1.replace("sits beside", "now sits beside"))
        # The same earlier rendering, but now a deliberate link: never an image.
        (repo / "Flip.md").write_text("Photo: [flip](/uploads/diagram.png)\n")
        git("add", "--all")
        git("commit", "--quiet", "-m", "second")
        second = git("rev-parse", "HEAD")
        source = ["import", "gollum", repo, "--source-base", BASE]

        def receipt_ids(text):
            return {key: rid for rid, key in re.findall(r"^([a-f0-9]{32})  (\S+)$", text, re.M)}

        def handles(opts):
            """Content hash of every cataloged file, by its bytes."""
            found = {}
            for line in run(files, *opts, "list").splitlines():
                found[fetch(opts, line.split()[0])] = re.search(r"[a-f0-9]{64}", line).group()
            return found

        def fetch(opts, file_id):
            return subprocess.run([files, *map(str, opts), "get", file_id, "@-"], env=env, cwd=root,
                                  capture_output=True, check=True).stdout

        def key(sha, path):
            return "rev/" + sha + "/" + hashlib.sha256(path.encode()).hexdigest()

        # -- A fresh import embeds every displayable image from its own commit.
        fresh = pile("fresh")
        imported = run(wiki, *fresh, *source)
        assert "embedded images" in imported, imported
        ids = receipt_ids(imported)
        by_bytes = handles(fresh)
        rack_v1, rack_v2, diagram = by_bytes[JPEG_V1], by_bytes[JPEG_V2], by_bytes[DIAGRAM]
        home_1 = run(wiki, *fresh, "export", "--exact", ids[key(first, "Home.md")])
        home_2 = run(wiki, *fresh, "export", "--exact", ids[key(second, "Home.md")])
        assert f'#image("files:{rack_v1}", alt: "in rack picture")' in home_1, home_1
        assert f'#image("files:{rack_v2}", alt: "in rack picture")' in home_2, home_2
        assert f'= #image("files:{diagram}", alt: "Logo")' in home_1
        assert f'[#image("files:{diagram}", alt: "cell")]' in home_1
        assert re.search(r'#link\("wiki:[0-9a-f]{32}"\)\[#image\("files:' + diagram + r'", alt: "button"\)\]', home_1)
        assert f'#figure(image("files:{diagram}", alt: "Framed diagram"), caption: [Framed diagram])' in home_1
        # Ordinary links, other hosts and undisplayable targets stay links.
        assert f'#link("files:{diagram}")[the diagram]' in home_1
        assert f'#link("{BASE}Other")[the other page]' in home_1
        assert '#link("https://elsewhere.example.invalid/x.png")[elsewhere]' in home_1
        assert re.search(r'#link\("files:[0-9a-f]{64}"\)\[a document\]', home_1)
        assert f'#link("{BASE}uploads/none.png")[missing]' in home_1
        # A page that did not change keeps the bytes of its own commit.
        assert f'#image("files:{rack_v1}", alt: "pinned")' in run(
            wiki, *fresh, "export", "--exact", ids[key(first, "Pinned.md")])
        guide = run(wiki, *fresh, "export", "--exact", ids[key(first, "Guide.adoc")])
        assert f'#image("files:{diagram}", alt: "Inline diagram")' in guide, guide
        assert f'#figure(image("files:{rack_v1}", alt: "Rack"), caption: [#text("Rack from the front")])' in guide
        assert '.Rack from the front' not in guide
        assert f'#figure(image("files:{diagram}", alt: "Absolute"), caption: [Titled diagram])' in guide
        for text in (home_1, home_2, guide):
            assert '"plan:' not in text
        again = run(wiki, *fresh, *source)
        assert again.rstrip().endswith("0 new revisions, 0 new provenance records, 0 new files, 0 publications"), again
        assert receipt_ids(again) == ids
        assert "0 heads with imported images" in run(wiki, *fresh, *source, "--repair-images")

        # -- The same history imported by the earlier converter.
        old = pile("old")
        legacy_wiki = env.get("OLD_WIKI")
        if legacy_wiki:
            earlier = run(legacy_wiki, *old, "import", "gollum", repo)
        else:
            plan_dir = root / "legacy-plan"
            run(sys.executable, "-I", HERE / "gollum-full-plan.py", repo, plan_dir)
            plan = json.loads((plan_dir / "plan.json").read_text())
            for commit in plan["commits"]:
                for revision in commit["revisions"]:
                    if revision.pop("images", None):
                        revision["content"] = revision.pop("legacy_content")
            (plan_dir / "plan.json").write_text(json.dumps(plan))
            earlier = run(wiki, *old, "import", "--plan", plan_dir / "plan.json")
        old_ids = receipt_ids(earlier)
        assert set(old_ids) == set(ids)
        home_rev = old_ids[key(second, "Home.md")]
        home_old = run(wiki, *old, "export", "--exact", home_rev)
        assert f'#link("{BASE}uploads/rack-photo.jpeg")[in rack picture]' in home_old
        assert "#image(" not in home_old
        # Re-importing with the new converter keeps the imported revisions.
        assert receipt_ids(run(wiki, *old, *source)) == old_ids

        # A person edits two imported pages afterwards.
        edited_rev = old_ids[key(first, "Edited.md")]
        edited_old = run(wiki, *old, "export", "--exact", edited_rev)
        lines = edited_old.split("\n")
        photo = next(i for i, line in enumerate(lines) if line.startswith("Photo:"))
        download = next(i for i, line in enumerate(lines) if line.startswith("Download:"))
        rack = next(i for i, line in enumerate(lines) if line.startswith("Rack:"))
        twice = next(i for i, line in enumerate(lines) if line.startswith("Twice:"))
        lines[photo], lines[download] = lines[download], lines[photo]  # swapped
        lines[rack] = lines[rack] + " (edited on the same line)"
        lines.insert(twice, lines[twice])  # duplicated
        lines.insert(0, "A paragraph a person added.\n")
        human = root / "human.typ"
        human.write_text("\n".join(lines))
        run(wiki, *old, "edit", edited_rev, "@" + str(human))
        guide_rev = old_ids[key(first, "Guide.adoc")]
        guide_old = run(wiki, *old, "export", "--exact", guide_rev)
        human.write_text(guide_old + "\nA closing note.\n")
        run(wiki, *old, "edit", guide_rev, "@" + str(human))
        listing = run(wiki, *old, "list")

        # -- Dry run: everything named, nothing written.
        dry = run(wiki, *old, *source, "--repair-images")
        assert "dry run: nothing published" in dry, dry
        assert run(wiki, *old, "list") == listing
        assert run(wiki, *old, "export", "--exact", home_rev) == home_old

        # -- Apply: one commit, a successor per corrected head.
        applied = run(wiki, *old, *source, "--repair-images", "--apply")
        assert applied.rstrip().endswith("1 publications"), applied
        corrected = {head: successor for head, successor in
                     re.findall(r"^CORRECT  ([0-9a-f]{32}) -> ([0-9a-f]{32})", applied, re.M)}
        assert home_rev in corrected and old_ids[key(first, "Pinned.md")] in corrected, applied
        assert old_ids[key(second, "Flip.md")] not in corrected and "Flip" not in applied, applied
        assert "#image(" in run(wiki, *fresh, "export", "--exact", ids[key(first, "Flip.md")])
        assert "#image(" not in run(wiki, *fresh, "export", "--exact", ids[key(second, "Flip.md")])
        home_new = run(wiki, *old, "export", "--exact", corrected[home_rev])
        # Page-link targets name revisions, whose ids include the author.
        unpinned = lambda text: re.sub(r'#link\("wiki:[0-9a-f]{32}"\)', '#link("wiki:")', text)
        assert unpinned(home_new) == unpinned(home_2), (home_new, home_2)
        assert run(wiki, *old, "export", "--exact", home_rev) == home_old

        edited_head = next((head for head in corrected if run(
            wiki, *old, "export", "--exact", head).startswith("A paragraph a person added.")), applied)
        assert edited_head in corrected, applied
        edited_new = run(wiki, *old, "export", "--exact", corrected[edited_head])
        assert edited_new.startswith("A paragraph a person added.")
        assert f'Photo: #image("files:{diagram}", alt: "same")' in edited_new, edited_new
        assert f'Download: #link("files:{diagram}")[same]' in edited_new
        assert "(edited on the same line)" in edited_new and f'#link("files:{rack_v1}")[rack]' in edited_new
        assert edited_new.count(f'Twice: #link("files:{diagram}")[twice]') == 2
        assert "its lines were edited or removed" in applied and "its lines occur more than once" in applied
        guide_head = next(head for head in corrected if run(
            wiki, *old, "export", "--exact", head).endswith("A closing note.\n"))
        guide_new = run(wiki, *old, "export", "--exact", corrected[guide_head])
        assert guide_new.endswith("A closing note.\n")
        assert f'#figure(image("files:{rack_v1}", alt: "Rack"), caption: [#text("Rack from the front")])' in guide_new
        assert '.Rack from the front' not in guide_new
        assert f'#figure(image("files:{diagram}", alt: "Absolute"), caption: [Titled diagram])' in guide_new

        # -- A second run finds nothing left and writes nothing.
        after = run(wiki, *old, "list")
        second_run = run(wiki, *old, *source, "--repair-images", "--apply")
        assert "CORRECT" not in second_run and " 0 corrected" in second_run, second_run
        assert "0 publications" in second_run
        assert run(wiki, *old, "list") == after
        # The corrected text also passes the ordinary edit path's checks.
        human.write_text(home_new + "\nChecked by hand.\n")
        run(wiki, *old, "edit", corrected[home_rev], "@" + str(human))
        human.write_text(home_new.replace(rack_v1 if rack_v1 in home_new else rack_v2, "0" * 64))
        run(wiki, *old, "edit", corrected[home_rev], "@" + str(human), succeeds=False)
        print("Images embed from their own commit; earlier imports are corrected in place, "
              "human edits are kept, ambiguous places are reported, and a second run writes nothing.")


if __name__ == "__main__":
    main()
