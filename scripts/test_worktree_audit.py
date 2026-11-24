#!/usr/bin/env python3

from __future__ import annotations

import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("worktree-audit.py")
REAL_GIT = shutil.which("git")


class WorktreeAuditTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.repo = self.root / "repo"
        self.git("init", "-q", "-b", "main", str(self.repo), cwd=self.root)
        self.git("config", "user.email", "audit-test@example.invalid")
        self.git("config", "user.name", "Audit Test")
        (self.repo / "tracked").write_text("initial\n")
        self.git("add", "tracked")
        self.git("commit", "-qm", "initial")

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def git(self, *args: str, cwd: Path | None = None) -> str:
        return subprocess.run(
            [REAL_GIT or "git", *args],
            cwd=cwd or self.repo,
            check=True,
            text=True,
            stdout=subprocess.PIPE,
        ).stdout.strip()

    def invoke(
        self, command: str, *extra: str, env: dict[str, str] | None = None
    ) -> subprocess.CompletedProcess[str]:
        invocation_env = os.environ.copy()
        if env:
            invocation_env.update(env)
        return subprocess.run(
            [str(SCRIPT), command, *extra, str(self.repo)],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=invocation_env,
        )

    def json_records(
        self, *extra: str, env: dict[str, str] | None = None
    ) -> tuple[subprocess.CompletedProcess[str], list[dict]]:
        result = self.invoke("report", "--jsonl", *extra, env=env)
        return result, [json.loads(line) for line in result.stdout.splitlines()]

    def test_quiet_due_and_versioned_report(self) -> None:
        due = self.invoke("due")
        self.assertEqual(due.returncode, 1, due.stderr)
        self.assertEqual(due.stdout, "")

        report, records = self.json_records()
        self.assertEqual(report.returncode, 0, report.stderr)
        self.assertEqual(len(records), 1)
        record = records[0]
        self.assertEqual(record["schema"], "faculties.worktree-audit")
        self.assertEqual(record["version"], 1)
        self.assertEqual(record["state"], "QUIET")
        self.assertEqual(record["base"]["ref"], "refs/heads/main")
        self.assertEqual(record["base"]["freshness"], "not_checked")
        self.assertEqual(record["remote_freshness"]["status"], "not_checked")
        self.assertEqual(record["live_process_custody"]["status"], "not_checked")
        self.assertEqual(
            {item["kind"] for item in record["lineage"]},
            {"local_head", "worktree_head"},
        )

    def test_reports_custody_sha_tree_aliases_and_nondefinitive_patch_evidence(
        self,
    ) -> None:
        initial = self.git("rev-parse", "HEAD")
        self.git("branch", "same-sha", initial)
        self.git("update-ref", "refs/remotes/origin/witness", initial)

        self.git("checkout", "-qb", "patch", initial)
        (self.repo / "patch").write_text("equivalent change\n")
        self.git("add", "patch")
        self.git("commit", "-qm", "subject patch")
        patch_sha = self.git("rev-parse", "HEAD")
        self.git("checkout", "-q", "main")
        (self.repo / "unrelated").write_text("base-only\n")
        self.git("add", "unrelated")
        self.git("commit", "-qm", "unrelated base change")
        self.git("cherry-pick", patch_sha)
        base = self.git("rev-parse", "HEAD")
        self.git("branch", "same-tree")
        self.git("checkout", "-q", "same-tree")
        self.git("commit", "--allow-empty", "-qm", "different narrative, exact tree")
        self.git("checkout", "-q", "main")

        unusual = self.root / "detached space\nline"
        self.git("worktree", "add", "-q", "--detach", str(unusual), base)
        (unusual / "dirty untracked").write_text("custody\n")

        due = self.invoke("due")
        self.assertEqual(due.returncode, 0, due.stderr)
        report, records = self.json_records()
        self.assertEqual(report.returncode, 0, report.stderr)
        record = records[0]
        self.assertEqual(record["state"], "ATTENTION")
        self.assertFalse(
            any(item["kind"] == "remote_ref" for item in record["lineage"])
        )

        same_sha = next(
            item for item in record["lineage"] if item["name"] == "refs/heads/same-sha"
        )
        self.assertIn(
            "refs/remotes/origin/witness", same_sha["exact_sha_witnesses"]["refs"]
        )

        same_tree = next(
            item for item in record["lineage"] if item["name"] == "refs/heads/same-tree"
        )
        self.assertTrue(same_tree["exact_tree_witnesses"]["base"])
        self.assertNotEqual(same_tree["sha"], record["base"]["sha"])

        patch = next(
            item for item in record["lineage"] if item["name"] == "refs/heads/patch"
        )["patch_evidence"]
        self.assertEqual(patch["status"], "observed")
        self.assertIn(patch_sha, patch["patch_equivalent_subject_commits"])
        self.assertFalse(patch["definitive_landed_disposition"])
        self.assertIn("non-definitive", patch["caveat"])

        detached = next(
            item for item in record["custody"] if item["path"] == str(unusual.resolve())
        )
        self.assertTrue(detached["detached"])
        self.assertTrue(detached["dirty"]["dirty"])
        human = self.invoke("report")
        self.assertIn("detached space\\nline", human.stdout)

    def test_remote_only_tips_change_the_normal_report_and_due_result(self) -> None:
        base = self.git("rev-parse", "main")
        prefix = "refs/remotes/origin/"
        self.git("update-ref", prefix + "main", base)
        self.git("symbolic-ref", prefix + "HEAD", prefix + "main")
        # Two related branch tips with no local branch or worktree. One forked
        # before main, the other after. Empty commits preserve the same tree.
        self.git("commit", "--allow-empty", "-qm", "advance main")
        self.git("update-ref", prefix + "main", "main")
        self.git("update-ref", prefix + "landed", base)
        self.git("checkout", "-q", "--detach", base)
        self.git("commit", "--allow-empty", "-qm", "first experiment")
        self.git("update-ref", prefix + "lane/first", "HEAD")
        self.git("update-ref", prefix + "lane/first-copy", "HEAD")
        self.git("checkout", "-q", "--detach", "main")
        self.git("commit", "--allow-empty", "-qm", "second experiment")
        self.git("update-ref", prefix + "lane/second", "HEAD")
        self.git("checkout", "-q", "main")

        result, records = self.json_records()
        self.assertEqual(result.returncode, 0, result.stderr)
        record = records[0]
        self.assertEqual(record["state"], "ATTENTION")
        rows = {row["name"]: row for row in record["lineage"] if row["kind"] == "remote_ref"}
        self.assertEqual(set(rows), {prefix + "lane/first", prefix + "lane/second"})
        self.assertEqual(rows[prefix + "lane/first"]["state"], "diverged")
        self.assertEqual(rows[prefix + "lane/second"]["state"], "descendant_of_base")
        self.assertEqual(
            rows[prefix + "lane/first"]["exact_sha_witnesses"]["refs"],
            [prefix + "lane/first", prefix + "lane/first-copy"],
        )
        self.assertEqual(rows[prefix + "lane/second"]["exact_sha_witnesses"]["worktrees"], [])
        self.assertEqual(len(record["attention"]), 2)
        self.assertEqual(record["remote_freshness"]["status"], "not_checked")
        human = self.invoke("report")
        self.assertIn("cached remote", human.stdout)
        self.assertEqual(self.invoke("due").returncode, 0)

        # Same tips on the owning host already have subjects, even with a
        # different local branch name or only a detached checkout.
        self.git("branch", "different-name", prefix + "lane/first")
        clean = self.root / "clean"
        dirty = self.root / "dirty"
        for path in (clean, dirty):
            self.git("worktree", "add", "-q", "--detach", str(path), prefix + "lane/second")
        (dirty / "untracked").write_text("only here\n")
        report, records = self.json_records()
        self.assertEqual(report.returncode, 0, report.stderr)
        lineage = records[0]["lineage"]
        self.assertFalse(any(row["kind"] == "remote_ref" for row in lineage))
        detached = [row for row in lineage if row.get("detached")]
        self.assertEqual({row["path"] for row in detached}, {str(clean.resolve()), str(dirty.resolve())})
        for row in detached:
            self.assertIn(prefix + "lane/second", row["exact_sha_witnesses"]["refs"])
        custody = {row["path"]: row["dirty"]["dirty"] for row in records[0]["custody"]}
        self.assertFalse(custody[str(clean.resolve())])
        self.assertTrue(custody[str(dirty.resolve())])

    def test_other_remotes_and_unrelated_history(self) -> None:
        base = self.git("rev-parse", "main")
        self.git("update-ref", "refs/remotes/origin/main", base)
        tree = self.git("rev-parse", "main^{tree}")
        orphan = self.git("commit-tree", tree, "-m", "independent root")
        self.git("update-ref", "refs/remotes/fork/topic", orphan)
        self.assertEqual(self.invoke("due").returncode, 1)
        self.git("update-ref", "refs/remotes/origin/topic", orphan)
        report, records = self.json_records()
        self.assertEqual(report.returncode, 0, report.stderr)
        rows = [row for row in records[0]["lineage"] if row["kind"] == "remote_ref"]
        self.assertEqual([row["name"] for row in rows], ["refs/remotes/origin/topic"])
        self.assertEqual(rows[0]["state"], "no_observed_merge_base")
        self.assertEqual(rows[0]["patch_evidence"]["status"], "not_checked")
        self.assertEqual(self.invoke("due").returncode, 0)

    def test_base_upstream_selects_its_namespace_and_uses_the_observed_sha(self) -> None:
        self.git("remote", "add", "team", "unused")
        self.git("remote", "add", "team/fork", "also-unused")
        base = self.git("rev-parse", "main")
        prefix = "refs/remotes/team/"
        self.git("update-ref", prefix + "stable", base)
        self.git("branch", "--set-upstream-to=team/stable", "main")
        self.git("commit", "--allow-empty", "-qm", "local commit")
        self.git("update-ref", prefix + "lane/nested/main", "HEAD")
        self.git("commit", "--allow-empty", "-qm", "branch advances")
        self.git("update-ref", prefix + "fork/topic", "HEAD")
        self.git("update-ref", prefix + "topic", "HEAD")
        self.git("checkout", "-qb", "topic", base)
        self.git("branch", "--set-upstream-to=team/topic", "topic")
        self.git("checkout", "-q", "main")
        self.git("commit", "--allow-empty", "-qm", "local integration")

        report, records = self.json_records()
        self.assertEqual(report.returncode, 0, report.stderr)
        self.assertEqual(records[0]["base"]["ref"], "refs/heads/main")
        self.assertEqual(records[0]["remote_subject_scope"]["namespace"], "team")
        self.assertFalse(any(row["kind"] == "remote_ref" for row in records[0]["lineage"]))

        # Relative to the older, explicitly selected cached base, the same
        # tips are outside it. A same-named tracking branch with a different
        # SHA does not account for the remote tip.
        report, records = self.json_records("--base", prefix + "stable")
        self.assertEqual(report.returncode, 0, report.stderr)
        rows = [row["name"] for row in records[0]["lineage"] if row["kind"] == "remote_ref"]
        self.assertEqual(rows, [prefix + "lane/nested/main", prefix + "topic"])
        self.assertEqual(records[0]["base"]["freshness"], "not_checked")

        report, records = self.json_records("--base", base)
        self.assertEqual(report.returncode, 0, report.stderr)
        self.assertIsNone(records[0]["remote_subject_scope"]["namespace"])
        self.assertTrue(any("coverage unresolved" in reason for reason in records[0]["attention"]))

    def test_discovery_permission_error_is_not_quiet_due(self) -> None:
        other = self.root / "inaccessible"
        self.git("init", "-q", str(other))
        other.chmod(0)
        try:
            if os.access(other, os.X_OK):
                self.skipTest("process can traverse mode-000 directories")
            for command in ("report", "due"):
                result = subprocess.run(
                    [str(SCRIPT), command, "--jsonl", "--root", str(self.root)],
                    text=True, capture_output=True,
                )
                self.assertEqual(result.returncode, 126, result.stderr)
                record = json.loads(result.stdout)
                self.assertEqual(record["state"], "INDETERMINATE")
                self.assertIn(str(other), record["diagnostic"])
        finally:
            other.chmod(0o700)

    def test_unreadable_loose_refs_from_linked_worktree(self) -> None:
        origin = "refs/remotes/origin/"
        self.git("update-ref", origin + "main", "main")
        self.git("pack-refs", "--all")
        tip = self.git("commit-tree", "main^{tree}", "-p", "main", "-m", "topic")
        self.git("update-ref", origin + "lane/topic", tip)
        linked = self.root / "linked"
        self.git("worktree", "add", "-q", "--force", str(linked), "main")
        refs = self.repo / ".git/refs"
        hidden = refs / "remotes/origin/lane"
        self.repo = linked  # Audit through the .git file, with shared refs elsewhere.
        for directory, mode in ((hidden, 0), (refs, 0o111)):
            directory.chmod(mode)
            try:
                if os.access(directory, os.R_OK):
                    self.skipTest("process can read inaccessible directories")
                for command in ("report", "due"):
                    result = self.invoke(command, "--jsonl")
                    self.assertEqual(result.returncode, 126, result.stdout)
                    record = json.loads(result.stdout)
                    self.assertEqual(record["state"], "INDETERMINATE")
                    self.assertIn(str(directory), record["diagnostic"])
            finally:
                directory.chmod(0o700)
        # Other loose namespaces do not participate in this audit.
        tags = refs / "tags"
        tags.mkdir(exist_ok=True)
        tags.chmod(0)
        try:
            report, records = self.json_records()
        finally:
            tags.chmod(0o700)
        self.assertEqual(report.returncode, 0, report.stdout)
        self.assertIn(origin + "lane/topic", [r["name"] for r in records[0]["lineage"]])

    def test_missing_parent_is_a_read_failure(self) -> None:
        parent = self.git("commit-tree", "main^{tree}", "-p", "main", "-m", "parent")
        tip = self.git("commit-tree", "main^{tree}", "-p", parent, "-m", "tip")
        self.git("update-ref", "refs/remotes/origin/topic", tip)
        (self.repo / ".git/objects" / parent[:2] / parent[2:]).unlink()
        for command in ("report", "due"):
            result = self.invoke(command, "--jsonl")
            self.assertEqual(result.returncode, 126, result.stdout)
            record = json.loads(result.stdout)
            self.assertEqual(record["state"], "INDETERMINATE")
            self.assertIn(parent, record["diagnostic"])

    def test_pruned_upstream_keeps_its_configured_namespace(self) -> None:
        self.git("remote", "add", "team/up", "unused")
        upstream = "refs/remotes/team/up/"
        self.git("update-ref", upstream + "stable", "main")
        self.git("branch", "--set-upstream-to=team/up/stable", "main")
        self.git("update-ref", "-d", upstream + "stable")
        tip = self.git("commit-tree", "main^{tree}", "-p", "main", "-m", "topic")
        self.git("update-ref", "refs/remotes/fork/other", tip)
        for expected in ([], [upstream + "topic"]):
            if expected:
                self.git("update-ref", upstream + "topic", tip)
            with self.subTest(expected=expected):
                result, records = self.json_records()
                self.assertEqual(result.returncode, 0, result.stdout)
                self.assertEqual(records[0]["remote_subject_scope"]["namespace"], "team/up")
                names = [r["name"] for r in records[0]["lineage"] if r["kind"] == "remote_ref"]
                self.assertEqual(names, expected)
                self.assertEqual(self.invoke("due").returncode, 0 if expected else 1)

    def test_nondefault_symbolic_refs_represent_and_witness_tips(self) -> None:
        origin = "refs/remotes/origin/"
        self.git("update-ref", origin + "main", "main")
        tip = self.git("commit-tree", "main^{tree}", "-p", "main", "-m", "topic")
        self.git("tag", "saved", tip)
        self.git("symbolic-ref", origin + "a-alias", "refs/tags/saved")
        self.git("pack-refs", "--all")
        witnesses = [origin + "a-alias"]
        for stage in ("tag", "other remote", "direct"):
            with self.subTest(stage=stage):
                if stage == "other remote":
                    self.git("update-ref", "refs/remotes/fork/other", tip)
                    self.git("symbolic-ref", origin + "a-alias", "refs/remotes/fork/other")
                    witnesses.append("refs/remotes/fork/other")
                if stage == "direct":
                    self.git("update-ref", origin + "topic", tip)
                    witnesses.append(origin + "topic")
                result, records = self.json_records()
                self.assertEqual(result.returncode, 0, result.stdout)
                rows = [r for r in records[0]["lineage"] if r["kind"] == "remote_ref"]
                name = origin + ("topic" if stage == "direct" else "a-alias")
                self.assertEqual([r["name"] for r in rows], [name])
                self.assertEqual(rows[0]["exact_sha_witnesses"]["refs"], sorted(witnesses))
                self.assertEqual(self.invoke("due").returncode, 0)

    def test_broken_cached_ref_is_indeterminate(self) -> None:
        ref = self.repo / ".git/refs/remotes/origin/topic"
        ref.parent.mkdir(parents=True)
        for content in ("broken", "1" * 40):
            with self.subTest(content=content):
                ref.write_text(content + "\n")
                report, records = self.json_records()
                self.assertEqual(report.returncode, 126, report.stderr)
                self.assertEqual(records[0]["state"], "INDETERMINATE")

    def test_missing_repository_takes_precedence_over_healthy_peer(self) -> None:
        missing = self.root / "missing"
        for command in ("report", "due"):
            result = self.invoke(command, "--jsonl", str(missing))
            self.assertEqual(result.returncode, 126)
            missing_record, healthy = map(json.loads, result.stdout.splitlines())
            self.assertEqual(missing_record["state"], "INDETERMINATE")
            self.assertIn(str(missing), missing_record["diagnostic"])
            self.assertEqual(healthy["state"], "QUIET")

    def test_origin_head_precedence_is_exact_and_freshness_is_not_checked(self) -> None:
        head = self.git("rev-parse", "HEAD")
        self.git("update-ref", "refs/remotes/origin/main", head)
        self.git("symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/main")
        report, records = self.json_records()
        self.assertEqual(report.returncode, 0, report.stderr)
        base = records[0]["base"]
        self.assertEqual(base["ref"], "refs/remotes/origin/HEAD")
        self.assertEqual(base["resolved_ref"], "refs/remotes/origin/main")
        self.assertEqual(base["sha"], head)
        self.assertEqual(base["freshness"], "not_checked")

    def test_non_origin_remote_head_is_a_portable_fork_base(self) -> None:
        head = self.git("rev-parse", "HEAD")
        self.git("branch", "-m", "main", "topic")
        self.git("update-ref", "refs/remotes/fork/main", head)
        self.git("symbolic-ref", "refs/remotes/fork/HEAD", "refs/remotes/fork/main")

        report, records = self.json_records()
        self.assertEqual(report.returncode, 0, report.stderr)
        base = records[0]["base"]
        self.assertEqual(base["ref"], "refs/remotes/fork/HEAD")
        self.assertEqual(base["resolved_ref"], "refs/remotes/fork/main")
        self.assertEqual(base["sha"], head)

    def test_complete_base_precedence_and_explicit_override(self) -> None:
        head = self.git("rev-parse", "HEAD")
        self.git("branch", "master", head)
        self.git("update-ref", "refs/remotes/origin/master", head)
        self.git("update-ref", "refs/remotes/origin/main", head)

        report, records = self.json_records()
        self.assertEqual(report.returncode, 0, report.stderr)
        self.assertEqual(records[0]["base"]["ref"], "refs/remotes/origin/main")

        explicit, records = self.json_records("--base", "refs/heads/master")
        self.assertEqual(explicit.returncode, 0, explicit.stderr)
        self.assertEqual(records[0]["base"]["ref"], "refs/heads/master")
        self.assertEqual(records[0]["base"]["selection"], "explicit")

        self.git("update-ref", "-d", "refs/remotes/origin/main")
        report, records = self.json_records()
        self.assertEqual(report.returncode, 0, report.stderr)
        self.assertEqual(records[0]["base"]["ref"], "refs/heads/main")

        self.git("branch", "-m", "main", "topic")
        report, records = self.json_records()
        self.assertEqual(report.returncode, 0, report.stderr)
        self.assertEqual(records[0]["base"]["ref"], "refs/remotes/origin/master")

        self.git("update-ref", "-d", "refs/remotes/origin/master")
        report, records = self.json_records()
        self.assertEqual(report.returncode, 0, report.stderr)
        self.assertEqual(records[0]["base"]["ref"], "refs/heads/master")

    def test_unresolved_base_is_indeterminate(self) -> None:
        other = self.root / "no-base"
        self.git("init", "-q", "-b", "topic", str(other), cwd=self.root)
        self.git("config", "user.email", "audit-test@example.invalid", cwd=other)
        self.git("config", "user.name", "Audit Test", cwd=other)
        (other / "file").write_text("one\n")
        self.git("add", "file", cwd=other)
        self.git("commit", "-qm", "topic", cwd=other)
        result = subprocess.run(
            [str(SCRIPT), "report", "--jsonl", str(other)],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        self.assertEqual(result.returncode, 126)
        record = json.loads(result.stdout)
        self.assertEqual(record["state"], "INDETERMINATE")
        self.assertIn("base unresolved", record["diagnostic"])

    def test_malformed_arguments_are_indeterminate_not_silently_not_due(self) -> None:
        result = self.invoke("due", "--definitely-not-an-option")
        self.assertEqual(result.returncode, 126)
        self.assertEqual(result.stdout, "")
        self.assertIn("invalid arguments", result.stderr)

    def test_ambient_git_repository_selection_is_ignored(self) -> None:
        other = self.root / "ambient"
        self.git("init", "-q", "-b", "main", str(other), cwd=self.root)
        self.git("config", "user.email", "audit-test@example.invalid", cwd=other)
        self.git("config", "user.name", "Audit Test", cwd=other)
        (other / "tracked").write_text("ambient clean\n")
        self.git("add", "tracked", cwd=other)
        self.git("commit", "-qm", "ambient", cwd=other)
        (self.repo / "dirty-target").write_text("must be observed\n")

        report, records = self.json_records(
            env={
                "GIT_DIR": str(other / ".git"),
                "GIT_WORK_TREE": str(other),
                "GIT_INDEX_FILE": str(other / ".git" / "index"),
            }
        )
        self.assertEqual(report.returncode, 0, report.stderr)
        self.assertEqual(records[0]["repository"], str(self.repo.resolve()))
        self.assertEqual(records[0]["state"], "ATTENTION")
        target = next(
            item
            for item in records[0]["custody"]
            if item["path"] == str(self.repo.resolve())
        )
        self.assertTrue(target["dirty"]["dirty"])

    def test_ref_race_is_reported_and_exits_126(self) -> None:
        shim = self.root / "shim"
        shim.mkdir()
        counter = self.root / "ref-count"
        head = self.git("rev-parse", "HEAD")
        wrapper = shim / "git"
        wrapper.write_text(
            "#!/bin/sh\n"
            "[ \"${GIT_OPTIONAL_LOCKS:-}\" = 0 ] || exit 98\n"
            f"real={shlex.quote(REAL_GIT or 'git')}\n"
            f"counter={shlex.quote(str(counter))}\n"
            "case \" $* \" in\n"
            "  *\\ for-each-ref\\ *)\n"
            "    n=0; [ ! -f \"$counter\" ] || n=$(cat \"$counter\")\n"
            "    n=$((n + 1)); printf '%s' \"$n\" > \"$counter\"\n"
            "    \"$real\" \"$@\"; rc=$?\n"
            f"    [ \"$n\" -lt 2 ] || printf 'refs/heads/raced\\t{head}\\t\\t\\n'\n"
            "    exit \"$rc\";;\n"
            "esac\n"
            "exec \"$real\" \"$@\"\n"
        )
        wrapper.chmod(0o755)
        env = {"PATH": os.pathsep.join([str(shim), os.environ.get("PATH", "")])}
        report, records = self.json_records(env=env)
        self.assertEqual(report.returncode, 126, report.stderr)
        self.assertEqual(records[0]["state"], "RACED")

    def test_worktree_dirtiness_race_is_reported_and_exits_126(self) -> None:
        shim = self.root / "status-shim"
        shim.mkdir()
        counter = self.root / "status-count"
        wrapper = shim / "git"
        wrapper.write_text(
            "#!/bin/sh\n"
            f"real={shlex.quote(REAL_GIT or 'git')}\n"
            f"counter={shlex.quote(str(counter))}\n"
            "case \" $* \" in\n"
            "  *\\ status\\ *)\n"
            "    n=0; [ ! -f \"$counter\" ] || n=$(cat \"$counter\")\n"
            "    n=$((n + 1)); printf '%s' \"$n\" > \"$counter\"\n"
            "    if [ \"$n\" -eq 2 ]; then printf '? raced\\0'; exit 0; fi;;\n"
            "esac\n"
            "exec \"$real\" \"$@\"\n"
        )
        wrapper.chmod(0o755)
        env = {"PATH": os.pathsep.join([str(shim), os.environ.get("PATH", "")])}
        report, records = self.json_records(env=env)
        self.assertEqual(report.returncode, 126, report.stderr)
        self.assertEqual(records[0]["state"], "RACED")
        self.assertIn("dirtiness changed", records[0]["diagnostic"])

    def test_every_git_command_has_optional_locks_disabled_and_is_read_only(
        self,
    ) -> None:
        shim = self.root / "logging-shim"
        shim.mkdir()
        log = self.root / "git-log"
        wrapper = shim / "git"
        wrapper.write_text(
            "#!/bin/sh\n"
            "[ \"${GIT_OPTIONAL_LOCKS:-}\" = 0 ] || exit 98\n"
            "[ \"${GIT_NO_LAZY_FETCH:-}\" = 1 ] || exit 97\n"
            f"printf '%s\\0' \"$@\" >> {shlex.quote(str(log))}\n"
            f"printf '\\n' >> {shlex.quote(str(log))}\n"
            f"exec {shlex.quote(REAL_GIT or 'git')} \"$@\"\n"
        )
        wrapper.chmod(0o755)
        env = {"PATH": os.pathsep.join([str(shim), os.environ.get("PATH", "")])}
        report = self.invoke("report", "--jsonl", env=env)
        self.assertEqual(report.returncode, 0, report.stderr)
        allowed = {
            "cherry",
            "for-each-ref",
            "merge-base",
            "remote",
            "rev-list",
            "rev-parse",
            "status",
            "worktree",
        }
        for raw_invocation in log.read_bytes().split(b"\n"):
            arguments = [part.decode() for part in raw_invocation.split(b"\0") if part]
            if not arguments:
                continue
            self.assertEqual(arguments[0], "-C")
            self.assertIn(arguments[2], allowed)
            if arguments[2] == "remote":
                self.assertEqual(arguments[2:], ["remote"])
            if arguments[2] == "worktree":
                self.assertEqual(arguments[3], "list")


if __name__ == "__main__":
    unittest.main()
