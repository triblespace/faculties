#!/usr/bin/env python3
"""Exercise the offline Gollum adapter with genuinely diverging Git histories."""
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


class GollumHistory(unittest.TestCase):
    def test_all_refs_and_original_page_identity(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            repo = root / "repo"
            repo.mkdir()

            def git(*args):
                return subprocess.check_output(
                    ["git", "-C", str(repo), "-c", "user.name=Fixture",
                     "-c", "user.email=fixture@example.invalid", *args],
                    stderr=subprocess.PIPE, text=True,
                ).strip()

            def commit(message):
                git("add", "-A")
                git("commit", "-qm", message)
                return git("rev-parse", "HEAD")

            git("init", "-qb", "main")
            (repo / "Home.md").write_text("One\n")
            (repo / "Link.md").write_text("[[Home]]\n")
            (repo / "branch").mkdir()
            (repo / "branch" / "Home.md").write_text("One\n")
            base = commit("two initially identical pages and a page link")
            git("checkout", "-qb", "left")
            (repo / "Home.md").write_text("Left\n")
            left = commit("left edit")
            (repo / "Left.md").write_text("[[Home]]\n")
            left_link = commit("left branch link")
            git("checkout", "-qb", "right", base)
            (repo / "Right.md").write_text("[[Home]]\n")
            right = commit("right branch link")
            git("checkout", "-q", "left")
            git("merge", "--no-ff", "-qm", "combine", "right")
            merge = git("rev-parse", "HEAD")
            git("mv", "Home.md", "Handbook.asciidoc")
            rename = commit("cross-format rename")
            git("checkout", "-q", "main")
            (repo / "Home.md").unlink()
            removal = commit("independent removal")
            git("checkout", "--orphan", "unrelated")
            git("rm", "-qrf", ".")
            (repo / "Unrelated.md").write_text("Independent root")
            orphan = commit("disconnected root")
            (repo / "manual-link").symlink_to("Unrelated.md")
            symlink = commit("original symlink")

            adapter = Path(__file__).with_name("gollum-full-plan.py")
            first = root / "first"
            second = root / "second"
            for output in (first, second):
                subprocess.run([sys.executable, str(adapter), str(repo), str(output)],
                               check=True, stdout=subprocess.PIPE)
            for file in first.rglob("*"):
                if file.is_file():
                    self.assertEqual(file.read_bytes(), (second / file.relative_to(first)).read_bytes())
            commits = {c["commit"]: c for c in json.loads((first / "plan.json").read_text())["commits"]}

            def rev(sha, path):
                return next(r for r in commits[sha]["revisions"] if r["path"] == path)

            def text(revision):
                return (first / revision["content"]).read_text()

            self.assertIn(rev(base, "Home.md")["key"], text(rev(right, "Right.md")))
            self.assertIn(rev(left, "Home.md")["key"], text(rev(left_link, "Left.md")))
            self.assertNotEqual(text(rev(base, "Home.md")), text(rev(base, "branch/Home.md")))
            self.assertEqual(set(rev(merge, "Home.md")["predecessors"]),
                             {rev(left, "Home.md")["key"], rev(base, "Home.md")["key"]})
            self.assertEqual(rev(rename, "Handbook.asciidoc")["predecessors"], [rev(merge, "Home.md")["key"]])
            self.assertIn("gelöscht", text(rev(removal, "Home.md")))
            self.assertNotIn("source", rev(removal, "Home.md"))
            self.assertNotIn("Originalquelle", text(rev(removal, "Home.md")))
            for sha, commit_row in commits.items():
                for revision in commit_row["revisions"]:
                    if "source" in revision:
                        oid = git("rev-parse", sha + ":" + revision["path"])
                        self.assertEqual(revision["source"], "source/" + oid)
                        self.assertIn('plan:source/' + oid + '")[Originalquelle]', text(revision))
            self.assertIn(orphan, commits)
            self.assertEqual((first / commits[symlink]["attachments"][0]["file"]).read_bytes(), b"Unrelated.md")

            # Adding a ref at an earlier commit can reorder topological emission.
            # Compare native inputs for each original occurrence, never filenames
            # or a globally latest version of the link target.
            git("checkout", "-qb", "new-from-base", base)
            (repo / "Later.md").write_text("[[Home]]\n")
            addition = commit("additional branch from old history")
            extended = root / "extended"
            subprocess.run([sys.executable, str(adapter), str(repo), str(extended)],
                           check=True, stdout=subprocess.PIPE)
            latest = {c["commit"]: c for c in json.loads((extended / "plan.json").read_text())["commits"]}
            self.assertIn(addition, latest)
            for sha, original in commits.items():
                self.assertEqual(len(original["revisions"]), len(latest[sha]["revisions"]))
                for before, after in zip(original["revisions"], latest[sha]["revisions"]):
                    self.assertEqual({k: v for k, v in before.items() if k != "content"},
                                     {k: v for k, v in after.items() if k != "content"})
                    self.assertEqual((first / before["content"]).read_bytes(),
                                     (extended / after["content"]).read_bytes())


if __name__ == "__main__":
    unittest.main()
