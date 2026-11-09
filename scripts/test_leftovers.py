#!/usr/bin/env python3
"""Black-box scanner regressions; every invocation uses a temporary HOME."""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("leftovers.sh")
OLD = 946684800  # 2000-01-01 UTC
TAG = (b"Signature: 8a477f597d28d172789f06886806bc55\n"
       b"# This file is a cache directory tag created by cargo.\n")


class LeftoversTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="leftovers-test-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.home = self.root / "home"
        self.home.mkdir()
        # One real allocated file clears the fixed 0.1 GB Cargo threshold.
        # Hard links reuse it, including a link outside the scanned HOME.
        self.payload = self.root / "payload"
        with self.payload.open("wb") as f:
            for _ in range(101):
                f.write(b"x" * 1_000_000)
        self.assertGreaterEqual(self.payload.stat().st_blocks * 512, 0.1e9)

    def target(self, name):
        path = self.home / name
        path.mkdir(parents=True)
        os.link(self.payload, path / "artifact")
        (path / "CACHEDIR.TAG").write_bytes(TAG)
        (path / ".rustc_info.json").write_text("{}")
        return path

    def age(self):
        for path, dirs, files in os.walk(self.home, topdown=False):
            for name in dirs + files:
                os.utime(Path(path) / name, (OLD, OLD), follow_symlinks=False)
            os.utime(path, (OLD, OLD))

    def report(self):
        result = subprocess.run(
            ["bash", str(SCRIPT), "--size-gb", "0"],
            env=dict(os.environ, HOME=str(self.home), TZ="UTC"),
            capture_output=True, text=True, timeout=5,
        )
        self.assertIn(result.returncode, (0, 1), result.stderr)
        self.assertEqual(result.stderr, "")
        return result.stdout

    def test_nested_targets_and_shared_inodes_have_independent_rows(self):
        first = self.target("project/target")
        second = self.target("other/target")
        os.link(self.payload, first / "same-inode-again")
        os.link(self.payload, self.home / "standalone")
        self.age()
        report = self.report()
        for target in (first, second):
            self.assertIn(
                f"clean   0.1 GB  last change 2000-01-01  {target}", report)
        self.assertIn("2 build outputs to clean (0.1 GB) of 2 found", report)
        self.assertIn("3 to decide (0.1 GB)", report)
        self.assertIn(
            f"decide  0.1 GB  last change 2000-01-01  {self.home / 'standalone'}",
            report)
        self.assertIn("rows and sections overlap", report)
        self.assertIn("not reclaimable", report)

    def test_regular_file_dates_and_symlink_traversal(self):
        old_file = self.home / "old-file"
        old_file.write_text("old")
        (self.home / "directory").mkdir()
        (self.home / "directory" / "link").symlink_to(self.payload)
        (self.home / "alias").symlink_to(self.root, target_is_directory=True)
        self.age()
        (self.home / "recent-file").write_text("recent")
        report = self.report()
        self.assertIn(f"last change 2000-01-01  {old_file}", report)
        self.assertNotIn("recent-file", report)
        self.assertNotIn(str(self.home / "alias"), report)
        self.assertIn(f"decide  0.0 GB  last change 2000-01-01  {self.home / 'directory'}", report)

    def test_recent_parent_still_reports_its_stale_target(self):
        target = self.target("project/target")
        self.age()
        (self.home / "project" / "active-work").write_text("recent")
        report = self.report()
        self.assertFalse(any(line.startswith("decide ") for line in report.splitlines()))
        self.assertIn(f"clean   0.1 GB  last change 2000-01-01  {target}", report)
        self.assertIn("1 build outputs to clean (0.1 GB) of 1 found", report)

    def test_holds_prune_targets_and_ancestors_before_detection(self):
        self.target("kept/target")
        held_target = self.target("project/target")
        skipped_target = self.target("skip-target")
        self.target("skip-ancestor/target")
        (held_target / ".keep").write_text("retain target")
        (skipped_target / ".leftovers-skip").touch()
        (self.home / "skip-ancestor" / ".leftovers-skip").touch()
        os.mkfifo(self.target("fifo-hold") / ".keep")
        (self.target("dangling-hold") / ".keep").symlink_to(self.root / "missing")
        self.age()
        # A new hold changes the root directory mtime, but is not activity.
        (self.home / "kept" / ".keep").write_text("retain history\nsecond line")
        report = self.report()
        self.assertIn(f"kept    0.1 GB  last change 2000-01-01  {self.home / 'kept'}  (retain history)", report)
        self.assertIn("0 build outputs to clean (0.0 GB) of 0 found", report)

    def test_only_regular_cargo_markers_are_read(self):
        for kind in ("symlink", "fifo", "directory"):
            target = self.target(kind)
            tag = target / "CACHEDIR.TAG"
            tag.unlink()
            if kind == "symlink":
                outside_tag = self.root / "outside-tag"
                outside_tag.write_bytes(TAG)
                tag.symlink_to(outside_tag)
            elif kind == "fifo":
                os.mkfifo(tag)
            else:
                tag.mkdir()
        target = self.target("rustc-symlink")
        (target / ".rustc_info.json").unlink()
        (target / ".rustc_info.json").symlink_to(self.payload)
        self.target("valid")
        self.age()
        report = self.report()
        self.assertIn("1 build outputs to clean (0.1 GB) of 1 found", report)
        self.assertEqual(sum(line.startswith("clean ") for line in report.splitlines()), 1)

    def test_descendant_holds_protect_the_whole_target(self):
        targets = []
        for name, marker in (("keep", ".keep"), ("skip", ".leftovers-skip")):
            target = self.target(f"{name}/target")
            archive = target / "archived"
            archive.mkdir()
            (archive / marker).touch()
            targets.append(target)
        self.age()
        report = self.report()
        self.assertFalse(any(line.startswith("clean ") for line in report.splitlines()))
        for target in targets:
            self.assertIn(f"kept    size not measured  {target}", report)
            self.assertIn(f"decide  0.1 GB  last change 2000-01-01  {target.parent}", report)
        self.assertIn("0 build outputs to clean (0.0 GB) of 2 found", report)
        self.assertIn("2 to decide (0.1 GB), 2 kept", report)

    @unittest.skipIf(os.geteuid() == 0, "root can read mode-000 files")
    def test_unreadable_keep_is_an_error_and_does_not_offer_cleanup(self):
        target = self.target("held")
        marker = target / ".keep"
        marker.write_text("keep")
        self.age()
        marker.chmod(0)
        self.addCleanup(marker.chmod, 0o600)
        report = self.report()
        self.assertIn(f"error   {target}  (could not read keep marker:", report)
        self.assertIn("0 to decide (0.0 GB), 0 kept, 1 unreadable", report)
        self.assertIn("0 build outputs to clean (0.0 GB) of 0 found", report)


if __name__ == "__main__":
    unittest.main()
