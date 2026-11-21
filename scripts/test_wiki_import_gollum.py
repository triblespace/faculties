#!/usr/bin/env python3
"""Exercise the shipped wiki import gollum entry point on local temporary data."""
import os
from pathlib import Path
import re
import subprocess
import tempfile


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
        (repo / "Home.md").write_text("# Welcome\n\n[[Guide]]\n")
        (repo / "Guide.md").write_text("First version\n")
        (repo / "guide file.bin").write_bytes(b"binary\0payload")
        git("add", "--all")
        commit("First version")
        (repo / "Guide.md").write_text("Second version\n")
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

        first = run(wiki, *opts, "import", "gollum", repo)
        second = run(wiki, *opts, "import", "gollum", repo)
        supplied = run(wiki, *opts, "import", "gollum", bundle)
        assert first.rstrip().endswith(b"2 publications"), first
        for replay in [second, supplied]:
            assert first.splitlines()[:-1] == replay.splitlines()[:-1]
            assert replay.rstrip().endswith(b"0 new revisions, 0 new provenance records, 0 new files, 0 publications")
        match = re.search(rb"Source bundle: ([a-f0-9]{32})", supplied)
        assert match
        assert run(files, *opts, "get", match[1].decode(), "@-") == bundle.read_bytes()
        original_records = {line for line in first.splitlines() if re.match(rb"^[a-f0-9]{32}  ", line)}
        (repo / "Guide.md").write_text("Third version after initial import.\n")
        git("add", "--all")
        commit("New guide")
        extended = run(wiki, *opts, "import", "gollum", repo)
        assert original_records and original_records.issubset(set(extended.splitlines()))
        assert extended.rstrip().endswith(b"2 publications")
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
        print("Direct repository and bundle imports converge, Files retains the original, and ordinary import passes.")


if __name__ == "__main__":
    main()
