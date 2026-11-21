#!/usr/bin/env python3
"""Exercise the shipped wiki import gollum entry point on local temporary data."""
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile


def mappings(receipt):
    return {key.decode(): rid.decode() for rid, key in
            re.findall(rb"^([a-f0-9]{32})  (.+)$", receipt, re.MULTILINE)}


def main():
    env = {k: v for k, v in os.environ.items()
           if k not in {"PILE", "PERSONA"} and not k.startswith(("TRIBLESPACE_", "FACULTIES_", "DRIVE_"))}
    wiki = env.get("WIKI", "wiki")
    files = env.get("FILES", "files")
    with tempfile.TemporaryDirectory(prefix="wiki import gollum ") as tmp:
        root = Path(tmp)
        repo = root / "studio wiki"
        bundle = root / "studio wiki.bundle"
        pile = root / "test.pile"
        key = root / "test.key"
        opts = ["--pile", pile, "--key", key]

        def run(*args, succeeds=True):
            result = subprocess.run(args, cwd=root, env=env, capture_output=True)
            if (result.returncode == 0) != succeeds:
                raise AssertionError(f"{args[0]} exited {result.returncode}: {result.stderr.decode(errors='replace')}")
            return result.stdout

        def git(*args):
            return run("git", "-C", repo, *args)

        def commit(message):
            git("-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                "-c", "commit.gpgSign=false", "commit", "--quiet", "-m", message)

        run("git", "init", "--quiet", "-b", "main", repo)
        (repo / "Home.md").write_text("# Welcome\n\n> [[Guide#one]]\n")
        (repo / "Guide.md").write_text("First version: [[Home]] and [section](#local-title)\n")
        (repo / "Handbook.asciidoc").write_text("= Handbook\n\nConsult link:Guide.md[Guide].\n")
        (repo / "History.textile").write_text("h1. Historical article\n")
        (repo / "redirects.gollum").write_text("/Start: /Home\n")
        (repo / "drawing.pdf").write_bytes(b"%PDF-1.4 fixture version 1")
        (repo / "past image.png").write_bytes(b"historical PNG fixture")
        (repo / "guide file.bin").write_bytes(b"binary\0payload")
        git("add", "--all")
        commit("First version")
        (repo / "Guide.md").write_text("Second version\n\n[Drawing](drawing.pdf)\n")
        (repo / "drawing.pdf").write_bytes(b"%PDF-1.4 fixture version 2")
        (repo / "past image.png").unlink()
        git("add", "--all")
        commit("Next version")
        git("-c", "pack.threads=1", "bundle", "create", bundle, "--all", "HEAD")
        run(env.get("TRIBLE", "trible"), "pile", "create", pile)
        run(env.get("TRIBLE", "trible"), "pile", "signing-key", "init", pile, "--key", key)
        run(env.get("BOOTSTRAP", "bootstrap"), *opts, "import")
        run(wiki, "import", "gollum", "--help")
        run(wiki, *opts, "import", "gollum", root/"missing", succeeds=False)
        invalid = root / "broken.bundle"
        invalid.write_bytes(b"invalid")
        run(wiki, *opts, "import", "gollum", invalid, succeeds=False)

        # A custom extract can name the same Git path and commit. No second
        # history or newly staged attachments should be published on ambiguity.
        custom = root / "custom"
        custom.mkdir()
        shutil.copy2(pile, custom / "test.pile")
        shutil.copy2(key, custom / "test.key")
        custom_opts = ["--pile", custom / "test.pile", "--key", custom / "test.key"]
        first_sha, author, date = git("log", "--reverse", "--format=%H%x09%an <%ae>%x09%aI").decode().splitlines()[0].split("\t")
        (custom / "extract.typ").write_text("A separately edited extract.")
        (custom / "plan.json").write_text(json.dumps({"commits": [{
            "commit": first_sha, "author": author, "date": date, "revisions": [{
                "key": "extract", "page": "custom", "path": "Guide.md",
                "title": "Guide", "content": "extract.typ", "predecessors": []
            }]}]}))
        run(wiki, *custom_opts, "import", "--plan", custom / "plan.json")
        before_files = run(files, *custom_opts, "list")
        before_pages = run(wiki, *custom_opts, "list")
        run(wiki, *custom_opts, "import", "gollum", repo, succeeds=False)
        assert before_files == run(files, *custom_opts, "list")
        assert before_pages == run(wiki, *custom_opts, "list")

        legacy = env.get("OLD_WIKI")
        legacy_opts = None
        if legacy:
            old = root / "legacy"
            old.mkdir()
            shutil.copy2(pile, old / "test.pile")
            shutil.copy2(key, old / "test.key")
            legacy_opts = ["--pile", old / "test.pile", "--key", old / "test.key"]
            old_receipt = run(legacy, *legacy_opts, "import", "gollum", bundle)
            inventory = run(files, *legacy_opts, "list")
            assert b"studio wiki.bundle" in inventory and b"Handbook.asciidoc" in inventory
            prior = {rid: run(legacy, *legacy_opts, "export", "--exact", rid)
                     for rid in mappings(old_receipt).values()}
            assert any(b"Originalquelle" not in body for body in prior.values())
            continued = run(wiki, *legacy_opts, "import", "gollum", bundle)
            assert mappings(old_receipt) == mappings(continued)
            assert b"0 new revisions, 0 new provenance records, 0 new files, 5 new source references, 1 publications" in continued, continued
            for rid, original in prior.items():
                assert run(wiki, *legacy_opts, "export", "--exact", rid) == original
                assert b"Originalquelle" in run(wiki, *legacy_opts, "show", "--exact", rid)
            assert run(wiki, *legacy_opts, "import", "gollum", repo).rstrip().endswith(b"0 publications")
            assert run(files, *legacy_opts, "list") == inventory
            inventory_legacy = inventory

        first = run(wiki, *opts, "import", "gollum", repo)
        second = run(wiki, *opts, "import", "gollum", repo)
        supplied = run(wiki, *opts, "import", "gollum", bundle)
        assert first.rstrip().endswith(b"2 publications"), first
        for replay in [second, supplied]:
            assert first.splitlines()[:-1] == replay.splitlines()[:-1]
            assert replay.rstrip().endswith(b"0 new revisions, 0 new provenance records, 0 new files, 0 publications")
        inventory = run(files, *opts, "list")
        names = sorted(line.split(b"  ")[1] for line in inventory.splitlines())
        assert names == sorted([b"drawing.pdf", b"drawing.pdf", b"past image.png",
                                b"guide file.bin", b"redirects.gollum", b"Home.md",
                                b"Guide.md", b"Guide.md", b"Handbook.asciidoc", b"History.textile"]), names
        payload_by_content = {}
        for line in inventory.splitlines():
            handle = re.search(rb"[a-f0-9]{64}", line).group().decode()
            payload_by_content[handle] = run(files, *opts, "get", line.split()[0].decode(), "@-")
        payloads = set(payload_by_content.values())
        assert b"historical PNG fixture" in payloads
        assert {b"%PDF-1.4 fixture version 1", b"%PDF-1.4 fixture version 2"} <= payloads
        exports = [run(wiki, *opts, "export", "--exact", rid).decode() for rid in mappings(first).values()]
        assert any("Historical article" in text for text in exports)
        assert any("Handbook" in text for text in exports)
        assert any(len(re.findall(r'#link\("files:', text)) > 1 for text in exports)
        assert any('#link("wiki:' in text for text in exports)
        paths = {hashlib.sha256(path.encode()).hexdigest(): path for path in
                 ("Home.md", "Guide.md", "Handbook.asciidoc", "History.textile")}
        for key, text in zip(mappings(first), exports):
            _, sha, path_key = key.split("/")
            source_pin, = re.findall(r'#link\("files:([a-f0-9]+)"\)\[Originalquelle\]', text)
            assert payload_by_content[source_pin] == git("show", sha + ":" + paths[path_key])
            assert '#link("plan:' not in text
            for pin in re.findall(r'#link\("files:([a-f0-9]+)"\)', text):
                assert pin.encode() in inventory
        original_records = {line for line in first.splitlines() if re.match(rb"^[a-f0-9]{32}  ", line)}
        (repo / "Guide.md").write_text("Third version after initial import.\n")
        git("add", "--all")
        commit("New guide")
        extended = run(wiki, *opts, "import", "gollum", repo)
        assert original_records and original_records.issubset(set(extended.splitlines()))
        assert extended.rstrip().endswith(b"2 publications")
        if legacy_opts:
            mixed = run(wiki, *legacy_opts, "import", "gollum", repo)
            assert all(mappings(mixed)[k] == v for k, v in mappings(old_receipt).items())
            assert b"1 new revisions, 1 new provenance records, 1 new files, 1 new source references, 2 publications" in mixed
            assert run(wiki, *legacy_opts, "import", "gollum", repo).rstrip().endswith(b"0 publications")
            assert set(run(files, *legacy_opts, "list").splitlines()) > set(inventory_legacy.splitlines())
        assert run(wiki, *opts, "import", "gollum", repo).rstrip().endswith(b"0 publications")

        # An unborn HEAD may coexist with populated refs in a bare repository.
        bare = root / "other.git"
        run("git", "clone", "--mirror", "--quiet", repo, bare)
        run("git", "-C", bare, "symbolic-ref", "HEAD", "refs/heads/unborn")
        assert original_records.issubset(set(run(wiki, *opts, "import", "gollum", bare).splitlines()))
        # Preserve the original wiki import PATH form beside its Gollum subcommand.
        plain = root / "plain.typ"
        plain.write_text("= Plain title\n\nAn independent page.\n")
        run(wiki, *opts, "import", plain)
        assert b"Plain title" in run(wiki, *opts, "list")
        print("Repo and bundle inputs converge. Every article cites its exact original, attachments round-trip, and prior history extends in place.")


if __name__ == "__main__":
    main()
