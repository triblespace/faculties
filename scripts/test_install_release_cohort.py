#!/usr/bin/env python3

from __future__ import annotations

import contextlib
import fcntl
import hashlib
from importlib.machinery import SourceFileLoader
import importlib.util
import json
import os
from pathlib import Path
import resource
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest import mock
from typing import Any


SCRIPT = Path(__file__).with_name("install-release-cohort")


class ReleaseCohortTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.source = self.root / "source"
        (self.source / "src").mkdir(parents=True)
        (self.source / "Cargo.toml").write_text(
            '[package]\nname = "cohort-fixture"\nversion = "0.1.0"\nedition = "2021"\n'
        )
        (self.source / "src" / "main.rs").write_text("fn main() {}\n")
        subprocess.run(
            ["cargo", "generate-lockfile", "--offline"], cwd=self.source, check=True
        )
        subprocess.run(["git", "init", "-q"], cwd=self.source, check=True)
        subprocess.run(
            ["git", "config", "user.email", "release-test@example.invalid"],
            cwd=self.source,
            check=True,
        )
        subprocess.run(
            ["git", "config", "user.name", "Release Test"], cwd=self.source, check=True
        )
        subprocess.run(["git", "add", "."], cwd=self.source, check=True)
        subprocess.run(["git", "commit", "-qm", "fixture"], cwd=self.source, check=True)
        self.build = self.root / "build"
        self.build.mkdir()
        self.prefix = self.root / "home" / ".local"
        self.binary("migrations", "fixture migrations")

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def binary(self, name: str, content: str) -> None:
        path = self.build / name
        path.write_text(f"#!/bin/sh\nprintf '%s\\n' '{content}'\n")
        path.chmod(0o755)

    def invoke(
        self, generation: str, *extra: str, env: dict[str, str] | None = None
    ) -> subprocess.CompletedProcess[str]:
        invocation_env = os.environ.copy()
        invocation_env.pop("GB10_EXACT_INVOCATION", None)
        invocation_env["PATH"] = os.pathsep.join(
            [str(self.prefix / "bin"), invocation_env.get("PATH", "")]
        )
        if env is not None:
            invocation_env.update(env)
        return subprocess.run(
            [
                str(SCRIPT),
                str(self.build),
                "--source-dir",
                str(self.source),
                "--prefix",
                str(self.prefix),
                "--generation",
                generation,
                "--no-default-features",
                *extra,
            ],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=invocation_env,
        )

    @staticmethod
    def exact_invocation() -> dict[str, Any]:
        return {
            "format": 1,
            "runner": {
                "path": "mary/scripts/gb10-runner",
                "revision": "a" * 40,
                "sha256": "b" * 64,
            },
            "source_cohort_sha256": "c" * 64,
            "command": ["cargo", "build", "--config", "", "--release", "--locked"],
        }

    def activate(
        self, generation: str, *, path: str | None = None
    ) -> subprocess.CompletedProcess[str]:
        env = os.environ.copy()
        env["PATH"] = path or os.pathsep.join(
            [str(self.prefix / "bin"), env.get("PATH", "")]
        )
        return subprocess.run(
            [
                str(SCRIPT),
                "--prefix",
                str(self.prefix),
                "--activate-staged",
                generation,
            ],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env,
        )

    def test_stages_manifest_links_and_atomic_generation_switch(self) -> None:
        self.binary("orient", "first orient")
        self.binary("mail", "first mail")
        staged = self.invoke("first", "--stage-only")
        self.assertEqual(staged.returncode, 0, staged.stderr)

        release = self.prefix / "lib" / "faculties" / "releases" / "first"
        current = self.prefix / "lib" / "faculties" / "current"
        self.assertFalse(os.path.lexists(current))
        activated = self.activate("first")
        self.assertEqual(activated.returncode, 0, activated.stderr)
        self.assertIn("already-running Faculties processes", activated.stderr)
        self.assertIn("lsof -a -d txt -p <pid>", activated.stderr)
        self.assertEqual(os.readlink(current), "releases/first")
        self.assertEqual(
            os.readlink(self.prefix / "bin" / "orient"),
            "../lib/faculties/current/bin/orient",
        )
        self.assertEqual(
            os.readlink(self.prefix / "bin" / "faculties-mail"),
            "../lib/faculties/current/bin/mail",
        )
        self.assertFalse(os.path.lexists(self.prefix / "bin" / "mail"))

        manifest = json.loads((release / "manifest.json").read_text())
        self.assertFalse(manifest["cargo"]["default_features"])
        self.assertEqual(manifest["cargo"]["features"], [])
        self.assertEqual(len(manifest["sources"]), 1)
        self.assertEqual(len(manifest["sources"][0]["revision"]), 40)
        self.assertEqual(len(manifest["sources"][0]["tree"]), 40)
        for binary in manifest["binaries"]:
            payload = (release / "bin" / binary["name"]).read_bytes()
            self.assertEqual(hashlib.sha256(payload).hexdigest(), binary["sha256"])

        for path in self.build.iterdir():
            path.unlink()
        self.binary("migrations", "second migrations")
        self.binary("orient", "second orient")
        self.assertEqual(self.invoke("second").returncode, 0)
        self.assertEqual(os.readlink(current), "releases/second")
        self.assertFalse(os.path.lexists(self.prefix / "bin" / "faculties-mail"))
        output = subprocess.run(
            [str(self.prefix / "bin" / "orient")],
            check=True,
            text=True,
            stdout=subprocess.PIPE,
        ).stdout
        self.assertEqual(output, "second orient\n")

    def test_records_allowlisted_build_environment_and_cargo_version(self) -> None:
        staged = self.invoke(
            "recipe-environment",
            "--stage-only",
            env={
                "RUSTFLAGS": "-Ctarget-cpu=native",
                "CARGO_INCREMENTAL": "0",
                "BUILD_SECRET": "must not enter provenance",
            },
        )
        self.assertEqual(staged.returncode, 0, staged.stderr)
        manifest = json.loads(
            (
                self.prefix
                / "lib"
                / "faculties"
                / "releases"
                / "recipe-environment"
                / "manifest.json"
            ).read_text()
        )
        recipe = manifest["build_recipe"]
        self.assertEqual(recipe["format"], 1)
        self.assertEqual(
            recipe["environment"],
            {"RUSTFLAGS": "-Ctarget-cpu=native", "CARGO_INCREMENTAL": "0"},
        )
        self.assertTrue(recipe["cargo_version_verbose"].startswith("cargo "))

    def test_exact_invocation_recipe_survives_stage_and_activation(self) -> None:
        receipt = self.exact_invocation()
        staged = self.invoke(
            "exact-recipe",
            "--stage-only",
            env={"GB10_EXACT_INVOCATION": json.dumps(receipt)},
        )
        self.assertEqual(staged.returncode, 0, staged.stderr)
        activated = self.activate("exact-recipe")
        self.assertEqual(activated.returncode, 0, activated.stderr)
        manifest = json.loads(
            (
                self.prefix
                / "lib"
                / "faculties"
                / "current"
                / "manifest.json"
            ).read_text()
        )
        recipe = manifest["build_recipe"]
        self.assertEqual(recipe["runner"], receipt["runner"])
        self.assertEqual(
            recipe["source_cohort_sha256"], receipt["source_cohort_sha256"]
        )
        self.assertEqual(recipe["command"], receipt["command"])

    def test_rejects_malformed_exact_invocation_hash(self) -> None:
        for field in ("runner", "source"):
            with self.subTest(field=field):
                receipt = self.exact_invocation()
                if field == "runner":
                    receipt["runner"]["sha256"] = "B" * 64
                else:
                    receipt["source_cohort_sha256"] = "c" * 63
                result = self.invoke(
                    f"malformed-{field}",
                    "--stage-only",
                    env={"GB10_EXACT_INVOCATION": json.dumps(receipt)},
                )
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("lowercase hexadecimal", result.stderr)
                self.assertFalse(
                    (
                        self.prefix
                        / "lib"
                        / "faculties"
                        / "releases"
                        / f"malformed-{field}"
                    ).exists()
                )

    def test_rejects_unknown_exact_invocation_fields(self) -> None:
        receipt = self.exact_invocation()
        receipt["surprise"] = "not part of format 1"
        result = self.invoke(
            "unknown-receipt-field",
            "--stage-only",
            env={"GB10_EXACT_INVOCATION": json.dumps(receipt)},
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("unexpected surprise", result.stderr)

    def test_rejects_partial_exact_recipe_during_activation(self) -> None:
        staged = self.invoke("partial-recipe", "--stage-only")
        self.assertEqual(staged.returncode, 0, staged.stderr)
        manifest_path = (
            self.prefix
            / "lib"
            / "faculties"
            / "releases"
            / "partial-recipe"
            / "manifest.json"
        )
        manifest = json.loads(manifest_path.read_text())
        manifest["build_recipe"]["runner"] = self.exact_invocation()["runner"]
        manifest_path.write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
        activated = self.activate("partial-recipe")
        self.assertNotEqual(activated.returncode, 0)
        self.assertIn("must be present together", activated.stderr)

    def test_activates_legacy_format_one_manifest_without_build_recipe(self) -> None:
        staged = self.invoke("legacy-recipe", "--stage-only")
        self.assertEqual(staged.returncode, 0, staged.stderr)
        manifest_path = (
            self.prefix
            / "lib"
            / "faculties"
            / "releases"
            / "legacy-recipe"
            / "manifest.json"
        )
        manifest = json.loads(manifest_path.read_text())
        del manifest["build_recipe"]
        manifest_path.write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
        activated = self.activate("legacy-recipe")
        self.assertEqual(activated.returncode, 0, activated.stderr)

    def test_dry_run_writes_nothing(self) -> None:
        self.binary("orient", "dry")
        result = self.invoke("dry", "--dry-run")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)["generation"], "dry")
        self.assertFalse(self.prefix.exists())

    def test_refuses_build_directory_without_migrations(self) -> None:
        (self.build / "migrations").unlink()
        self.binary("orient", "incomplete")
        result = self.invoke("incomplete", "--dry-run")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("required cohort binary is absent", result.stderr)
        self.assertIn("cargo build --release --workspace --bins", result.stderr)
        self.assertFalse(self.prefix.exists())

    def test_refuses_dirty_sources_and_unmanaged_commands(self) -> None:
        self.binary("orient", "one")
        (self.source / "src" / "main.rs").write_text("fn main() { panic!() }\n")
        dirty = self.invoke("dirty")
        self.assertNotEqual(dirty.returncode, 0)
        self.assertIn("source repository is dirty", dirty.stderr)

        (self.source / "src" / "main.rs").write_text("fn main() {}\n")
        (self.prefix / "bin").mkdir(parents=True)
        (self.prefix / "bin" / "orient").write_text("mine\n")
        collision = self.invoke("collision")
        self.assertNotEqual(collision.returncode, 0)
        self.assertIn("refusing to replace unmanaged command", collision.stderr)

    def test_ignores_untracked_build_and_worktree_litter(self) -> None:
        self.binary("orient", "one")
        target = self.source / "target" / "debug"
        target.mkdir(parents=True)
        (target / "generated").write_text("litter\n")
        worktree = self.source / ".claude" / "worktrees" / "agent"
        worktree.mkdir(parents=True)
        (worktree / "scratch.rs").write_text("outside selected checkout\n")
        clean = self.invoke("litter", "--dry-run")
        self.assertEqual(clean.returncode, 0, clean.stderr)

    def test_refuses_activation_when_an_earlier_path_entry_shadows_cohort(self) -> None:
        self.binary("orient", "managed orient")
        staged = self.invoke("shadowed", "--stage-only")
        self.assertEqual(staged.returncode, 0, staged.stderr)

        stale_bin = self.root / "home" / ".cargo" / "bin"
        stale_bin.mkdir(parents=True)
        stale_orient = stale_bin / "orient"
        stale_orient.write_text("#!/bin/sh\nprintf 'stale orient\\n'\n")
        stale_orient.chmod(0o755)
        path = os.pathsep.join(
            [str(stale_bin), str(self.prefix / "bin"), os.environ.get("PATH", "")]
        )

        activated = self.activate("shadowed", path=path)
        self.assertNotEqual(activated.returncode, 0)
        self.assertIn("PATH shadows managed Faculties command 'orient'", activated.stderr)
        self.assertIn(str(stale_orient), activated.stderr)
        self.assertFalse(
            os.path.lexists(self.prefix / "lib" / "faculties" / "current")
        )


MAP_ONLY = """
import ctypes, os, sys
path = sys.stdin.readline().strip()
libc = ctypes.CDLL(None, use_errno=True)
libc.mmap.restype = ctypes.c_void_p
libc.mmap.argtypes = [ctypes.c_void_p, ctypes.c_size_t] + [ctypes.c_int] * 3 + [ctypes.c_long]
descriptor = os.open(path, os.O_RDONLY)
assert libc.mmap(None, 4096, 1, 2, descriptor, 0) not in (None, ctypes.c_void_p(-1).value)
os.close(descriptor)
print("ready", flush=True)
sys.stdin.read()
"""
OPEN_ONLY = """
import sys
held = open(sys.stdin.readline().strip())
print("ready", flush=True)
sys.stdin.read()
"""
OPEN_UNREADABLE = """
import ctypes, sys
held = open(sys.stdin.readline().strip())
ctypes.CDLL(None).prctl(4, 0, 0, 0, 0)  # PR_SET_DUMPABLE: /proc closes to everyone but root
print("ready", flush=True)
sys.stdin.read()
"""


def limit_memory() -> None:
    """A script that reads without end fails here instead of taking the machine."""
    resource.setrlimit(resource.RLIMIT_AS, (2 << 30, 2 << 30))


def load_script():
    loader = SourceFileLoader("install_release_cohort", str(SCRIPT))
    module = importlib.util.module_from_spec(importlib.util.spec_from_loader(loader.name, loader))
    with mock.patch.object(sys, "dont_write_bytecode", True):
        loader.exec_module(module)
    return module


class PruneTest(unittest.TestCase):
    """Generations are written directly: a prune reads manifests, not binaries."""

    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        self.prefix = self.root / "prefix"
        self.lib = self.prefix / "lib" / "faculties"
        self.releases = self.lib / "releases"
        self.releases.mkdir(parents=True)
        (self.prefix / "bin").mkdir()
        # Names no real process mentions, in order of staging.
        stem = f"g{os.getpid()}x{self.root.name.replace('_', '-')}"
        self.a, self.b, self.c, self.d = (f"{stem}-{letter}" for letter in "abcd")
        for hour, name in enumerate((self.a, self.b, self.c, self.d)):
            self.generation(name, hour)
        shutil.rmtree(self.releases / self.d)
        self.link("current", self.c)
        self.link("previous", self.b)

    def generation(self, name: str, hour: int) -> Path:
        directory = self.releases / name
        (directory / "bin").mkdir(parents=True)
        tool = directory / "bin" / "tool"
        shutil.copy(shutil.which("sleep"), tool)
        (directory / "Cargo.lock").write_text(f"lock of {name}\n")
        manifest = {
            "format": 1,
            "generation": name,
            "created_utc": f"2026-11-25T{hour:02}:00:00.5+00:00",
            "cargo": {"lockfile_sha256": hashlib.sha256(f"lock of {name}\n".encode()).hexdigest()},
            "sources": [{"path": ".", "revision": "a" * 40}],
            "binaries": [
                {
                    "name": "tool",
                    "link_name": "tool",
                    "size": tool.stat().st_size,
                    "sha256": hashlib.sha256(tool.read_bytes()).hexdigest(),
                }
            ],
        }
        (directory / "manifest.json").write_text(json.dumps(manifest))
        return directory

    def link(self, name: str, generation: str) -> None:
        (self.lib / name).unlink(missing_ok=True)
        os.symlink(f"releases/{generation}", self.lib / name)

    def names(self) -> set[str]:
        return {path.name for path in self.releases.iterdir()}

    def script(self, *arguments: str, path: Path | None = None) -> subprocess.CompletedProcess[str]:
        env = {name: value for name, value in os.environ.items() if name != "OLDPWD"}
        first = [str(path)] if path else []
        env["PATH"] = os.pathsep.join([*first, str(self.prefix / "bin"), env["PATH"]])
        return subprocess.run(
            [sys.executable, str(SCRIPT), "--prefix", str(self.prefix), *arguments],
            text=True, capture_output=True, env=env, cwd=self.root, timeout=60,
            preexec_fn=limit_memory if sys.platform == "linux" else None,
        )

    def pruned(self, *arguments: str, path: Path | None = None) -> str:
        result = self.script(*(arguments or ("--prune-only",)), path=path)
        self.assertEqual(result.returncode, 0, result.stderr)
        return result.stdout

    def line(self, output: str, name: str) -> str:
        (line,) = [line for line in output.splitlines() if name in line.split(":")[0]]
        return line

    @contextlib.contextmanager
    def process(self, *args: str, feed: Path | None = None, **options):
        started = subprocess.Popen(
            args, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, **options
        )
        try:
            if feed:
                started.stdin.write(f"{feed}\n")
                started.stdin.flush()
                self.assertEqual(started.stdout.readline(), "ready\n")
            yield started
        finally:
            started.kill()
            started.communicate()

    def test_keeps_current_and_previous_and_retires_the_rest(self) -> None:
        manifest = (self.releases / self.a / "manifest.json").read_bytes()
        output = self.pruned()
        self.assertEqual(self.names(), {self.b, self.c})
        self.assertEqual(self.line(output, self.a), f"removed {self.a}")
        self.assertEqual(self.line(output, self.b), f"kept {self.b}: previous")
        self.assertEqual(self.line(output, self.c), f"kept {self.c}: current")
        self.assertIn("free space by df: ", output)
        (retired,) = (self.lib / "retired").iterdir()
        self.assertTrue(retired.name.startswith(f"{self.a}-"))
        self.assertEqual((retired / "manifest.json").read_bytes(), manifest)
        self.assertEqual((retired / "Cargo.lock").read_text(), f"lock of {self.a}\n")
        self.assertEqual({path.name for path in retired.iterdir()}, {"manifest.json", "Cargo.lock"})

    def test_dry_run_removes_and_writes_nothing(self) -> None:
        output = self.pruned("--prune-dry-run")
        self.assertEqual(self.line(output, self.a), f"would remove {self.a}")
        self.assertEqual(self.names(), {self.a, self.b, self.c})
        self.assertFalse((self.lib / "retired").exists())

    def test_prune_flags_stand_alone(self) -> None:
        stand_alone = [["--dry-run"], ["--stage-only"], ["--generation", "x"], ["build"]]
        for extra in (*stand_alone, ["--activate-staged", self.a], ["--no-prune"]):
            for only in ("--prune-only", "--prune-dry-run"):
                result = self.script(only, *extra)
                self.assertIn(result.returncode, (1, 2), extra)
                self.assertIn("--prune", result.stderr)
        self.assertEqual(self.names(), {self.a, self.b, self.c})
        self.assertEqual(os.readlink(self.lib / "current"), f"releases/{self.c}")

    def test_unrecorded_previous_is_guessed_and_said(self) -> None:
        for previous in (None, self.c):
            with self.subTest(previous=previous):
                (self.lib / "previous").unlink(missing_ok=True)
                if previous:
                    self.link("previous", previous)
                output = self.pruned("--prune-dry-run")
                self.assertIn("none is recorded", self.line(output, self.b))
                self.assertIn("would remove", self.line(output, self.a))

    def test_keeps_what_is_newer_or_undated(self) -> None:
        self.generation(self.d, 9)
        for path, change in ((self.a, {"created_utc": "yesterday"}), (self.b, {"generation": "other"})):
            manifest = self.releases / path / "manifest.json"
            manifest.write_text(json.dumps({**json.loads(manifest.read_text()), **change}))
        (self.lib / "previous").unlink()
        output = self.pruned()
        self.assertEqual(self.names(), {self.a, self.b, self.c, self.d})
        self.assertEqual(self.line(output, self.d), f"kept {self.d}: staged after the current")
        for name in (self.a, self.b):
            self.assertIn("no creation time", self.line(output, name))

    def test_refuses_without_a_readable_current(self) -> None:
        for target in (None, "releases/absent", f"elsewhere/{self.c}", "releases/.."):
            with self.subTest(target=target):
                (self.lib / "current").unlink(missing_ok=True)
                if target:
                    os.symlink(target, self.lib / "current")
                self.assertEqual(self.script("--prune-only").returncode, 1)
                self.assertEqual(self.names(), {self.a, self.b, self.c})

    def test_every_kind_of_live_holder_is_kept_and_named(self) -> None:
        linux = sys.platform == "linux"
        tool = self.releases / self.a / "bin" / "tool"
        python = sys.executable
        for kind, mac, args, options in (
            ("executable, mapped file", "executable", ["sleep", "60"], {"executable": str(tool)}),
            ("mapped file", "mapped", [python, "-c", MAP_ONLY], {"feed": tool}),
            ("open file", "open file", [python, "-c", OPEN_ONLY], {"feed": tool}),
            ("working directory", "working directory", ["cat"], {"cwd": tool.parent}),
            ("environment", "environment", [python, "-c", "input()"], {"env": {"TOOL": str(tool)}}),
            ("command line", "command line", ["sh", "-c", "read x; read y", str(tool)], {}),
        ):
            options.setdefault("env", {"PATH": os.environ["PATH"]})
            with self.subTest(kind=kind), self.process(*args, **options) as holder:
                line = self.line(self.pruned(), self.a)
                self.assertRegex(line, rf"kept .*held by pid {holder.pid} \w+")
                self.assertIn(f"({kind})" if linux else mac, line)
        self.assertEqual(self.line(self.pruned(), self.a), f"removed {self.a}")

    @unittest.skipUnless(sys.platform == "linux", "file leases")
    def test_a_holder_nobody_can_read_is_kept(self) -> None:
        tool = self.releases / self.a / "bin" / "tool"
        with self.process(sys.executable, "-c", OPEN_UNREADABLE, feed=tool) as holder:
            with self.assertRaises(PermissionError):
                os.listdir(f"/proc/{holder.pid}/fd")
            output = self.pruned()
            self.assertIn("tool is in use", self.line(output, self.a))
            self.assertRegex(output, r"not readable: .* of [1-9]\d* processes")
            unread: set[str] = set()
            list(load_script().linux_holders(unread))
            self.assertIn(str(holder.pid), unread)
            for pid in filter(str.isdigit, os.listdir("/proc")):
                with contextlib.suppress(OSError):
                    if not Path("/proc", pid, "maps").read_bytes():  # kernel threads
                        self.assertNotIn(pid, unread)
        self.assertEqual(self.line(self.pruned(), self.a), f"removed {self.a}")

    @unittest.skipUnless(Path("/run/systemd/system").is_dir(), "systemd")
    def test_loaded_units_hold_and_unreadable_ones_hold_everything(self) -> None:
        stub = self.root / "stub"
        stub.mkdir()
        systemctl = stub / "systemctl"
        for manager in ("--user", "--system"):
            systemctl.write_text(
                "#!/bin/sh\n"
                f'[ "$1" = {manager} ] || exit 0\n'
                # What systemctl does: without --all a stopped unit is not shown.
                'case "$*" in *--all*) ;; *) exit 0 ;; esac\n'
                f"printf 'Id=other.service\\n\\nId=pin.service\\nEnvironment=PATH={self.releases / self.a}/bin\\n'\n"
            )
            systemctl.chmod(0o755)
            line = self.line(self.pruned(path=stub), self.a)
            self.assertEqual(line, f"kept {self.a}: held by unit pin.service (loaded)")
        systemctl.write_text("#!/bin/sh\necho 'no bus' >&2\nexit 1\n")
        self.assertIn("cannot be read: systemctl", self.line(self.pruned(path=stub), self.a))
        # Output without end: stopped at the cap, and everything is kept.
        systemctl.write_text("#!/bin/sh\nexec cat /dev/zero\n")
        self.assertIn("cannot be read: more than", self.line(self.pruned(path=stub), self.a))
        self.assertEqual(self.names(), {self.a, self.b, self.c})

    def test_macos_holders(self) -> None:
        module = load_script()
        me = os.getpid()
        services = "x = {\n\tservices = {\n\t\t 5 - \tcom.apple.a\n\t\t 0 - \tsh.pin\n\t}\n\tservices = {\n"
        printed = {
            f"user/{os.getuid()}": services.replace("sh.pin", "sh.other").encode(),
            f"gui/{os.getuid()}": services.encode(),
            f"user/{os.getuid()}/sh.other": b"program = /p/releases/B/bin/tool",
            f"gui/{os.getuid()}/sh.pin": b"program = /p/releases/A/bin/tool",
        }

        def output(*args: str) -> bytes:
            if args[0] == "ps":
                return b"  7 tool X=/p/releases/A/bin\n"
            if args[0] == "lsof":
                return lsof.encode()
            if args[2] not in printed:
                raise OSError("no such domain")
            return printed[args[2]]

        lsof = f"p{me}\ncpython\nfcwd\nn/me\np9\nctool\nftxt\nn/p/A\nf3\nn/p/B\nfcwd\nn/p\n"
        with mock.patch.object(module, "output", output):
            seen = [(who, how) for who, how, _ in module.macos_holders(set())]
            self.assertEqual(
                seen,
                [
                    ("pid 9 tool", "executable or mapped file"),
                    ("pid 9 tool", "open file"),
                    ("pid 9 tool", "working directory"),
                    ("pid 7", "command line or environment"),
                    ("agent sh.pin", "loaded"),
                    ("agent sh.other", "loaded"),
                ],
            )
            # A label that cannot be printed, no domain, or a blind lsof: not "nobody".
            for lost in (f"gui/{os.getuid()}/sh.pin", f"gui/{os.getuid()}"):
                del printed[lost]
                with self.assertRaises(OSError):
                    list(module.macos_holders(set()))
            lsof = "p9\nctool\n"
            with self.assertRaises(OSError):
                next(module.macos_holders(set()))

    def test_a_stopped_consumer_is_spared_only_by_a_keep_file(self) -> None:
        wrapper = self.root / "wrapper"
        wrapper.write_text(f"#!/bin/sh\nexec {self.releases / self.a}/bin/tool 60\n")
        wrapper.chmod(0o755)
        keep = self.releases / self.a / ".keep"
        keep.write_text("desk: notification owner on the laptop\nuntil it moves\n")
        with self.process(str(wrapper)):
            pass
        expected = f"kept {self.a}: .keep: desk: notification owner on the laptop until it moves"
        self.assertEqual(self.line(self.pruned(), self.a), expected)
        # It disturbs neither a rollback to that generation nor the way back.
        for name in (self.a, self.c):
            activated = self.script("--activate-staged", name)
            self.assertEqual(activated.returncode, 0, activated.stderr)
        keep.unlink()
        self.link("previous", self.b)
        self.assertEqual(self.line(self.pruned(), self.a), f"removed {self.a}")

    def test_a_keep_that_is_no_plain_file_still_keeps(self) -> None:
        keep = self.releases / self.a / ".keep"
        big = self.root / "big"
        with big.open("wb") as file:
            file.truncate(8 << 30)
        for make in (
            lambda: os.symlink("/dev/zero", keep),
            lambda: os.symlink(self.root / "absent", keep),
            lambda: os.mkfifo(keep),
            keep.mkdir,
            lambda: os.rename(big, keep),
        ):
            make()
            self.assertIn(".keep: cannot be read (", self.line(self.pruned(), self.a))
            keep.rmdir() if keep.is_dir() else keep.unlink()
        self.assertEqual(self.line(self.pruned(), self.a), f"removed {self.a}")

    def test_leaves_what_is_not_a_generation_and_follows_no_symlink(self) -> None:
        outside = self.root / "outside"
        shutil.copytree(self.releases / self.a, outside)
        os.symlink(outside, self.releases / "linked")
        os.symlink(outside, self.releases / self.a / "bin" / "trap")
        os.symlink(outside, self.releases / self.a / "trap")
        (self.releases / "file").write_text("x")
        (self.releases / "bare" / "bin").mkdir(parents=True)
        shutil.copytree(outside, self.releases / ".staging")
        shutil.copytree(outside, self.releases / "borrowed")
        (self.releases / "borrowed" / "manifest.json").unlink()
        os.symlink(outside / "manifest.json", self.releases / "borrowed" / "manifest.json")
        output = self.pruned()
        for name, reason in (
            ("linked", "not a generation directory"),
            ("file", "not a generation directory"),
            ("bare", "no readable manifest.json"),
            ("borrowed", "no readable manifest.json"),
            (".staging", "not a generation directory"),
        ):
            self.assertIn(f"left {name}: {reason}", output)
        self.assertNotIn(self.a, self.names())
        self.assertEqual(
            {str(path.relative_to(outside)) for path in outside.rglob("*")},
            {"bin", "bin/tool", "Cargo.lock", "manifest.json"},
        )

    def test_refuses_when_releases_is_a_symlink(self) -> None:
        self.releases.rename(self.root / "moved")
        os.symlink(self.root / "moved", self.releases)
        self.assertEqual(self.script("--prune-only").returncode, 1)
        self.assertEqual(self.names(), {self.a, self.b, self.c})

    @unittest.skipIf(os.getuid() == 0, "root writes anywhere")
    def test_a_removal_that_fails_half_way_is_reported_and_finished_later(self) -> None:
        self.link("current", self.c)
        (self.releases / self.a / "aaa").mkdir()
        (self.releases / self.a / "aaa" / "x").write_text("x")
        locked = self.releases / self.a / "bin"
        locked.chmod(0o500)
        self.addCleanup(lambda: locked.exists() and locked.chmod(0o700))
        self.generation(self.d, 9)
        activated = self.script("--activate-staged", self.d)
        self.assertEqual(activated.returncode, 0, activated.stderr)
        self.assertIn(f"could not remove {self.a}: ", activated.stderr)
        self.assertIn(f"activated Faculties release {self.d}", activated.stdout)
        # Half gone, the manifest still there, and the next one was still removed.
        left = {path.name for path in (self.releases / self.a).iterdir()}
        self.assertEqual(left, {"bin", "manifest.json"})
        self.assertEqual(self.names(), {self.a, self.c, self.d})
        self.assertEqual(self.script("--prune-only").returncode, 1)
        locked.chmod(0o700)
        self.assertEqual(self.line(self.pruned(), self.a), f"removed {self.a}")
        self.assertEqual(len([p for p in (self.lib / "retired").iterdir() if p.name.startswith(self.a)]), 1)

    def test_activations_record_previous_and_prune(self) -> None:
        shutil.rmtree(self.releases / self.a)
        (self.lib / "current").unlink()
        (self.lib / "previous").unlink()
        e = self.d + "e"
        self.generation(self.a, 0)
        self.generation(self.d, 9)
        self.generation(e, 10)

        def activate(name: str, *extra: str) -> tuple[str, str | None, set[str]]:
            result = self.script("--activate-staged", name, *extra)
            self.assertEqual(result.returncode, 0, result.stderr)
            previous = self.lib / "previous"
            return (
                os.readlink(self.lib / "current")[9:],
                os.readlink(previous)[9:] if previous.is_symlink() else None,
                self.names(),
            )

        a, b, c, d = self.a, self.b, self.c, self.d
        self.assertEqual(activate(a), (a, None, {a, b, c, d, e}))
        self.assertEqual(activate(c, "--no-prune"), (c, a, {a, b, c, d, e}))
        # Two in a row: the second drops what neither is nor was current.
        self.assertEqual(activate(d), (d, c, {c, d, e}))
        # The same again changes nothing.
        self.assertEqual(activate(d), (d, c, {c, d, e}))
        # A rollback flips the two; what is newer stays until the next one forward.
        self.assertEqual(activate(c), (c, d, {c, d, e}))
        self.pruned()
        self.assertEqual(self.names(), {c, d, e})
        self.assertEqual(activate(e), (e, c, {c, e}))

    def test_activation_dry_run_prunes_nothing(self) -> None:
        result = self.script("--activate-staged", self.b, "--dry-run")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.names(), {self.a, self.b, self.c})

    def test_a_prune_that_refuses_does_not_fail_the_activation(self) -> None:
        manifest = self.releases / self.b / "manifest.json"
        manifest.write_text(manifest.read_text().replace("2026-11-25", "yesterday"))
        result = self.script("--activate-staged", self.b)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("warning: not pruned: ", result.stderr)
        self.assertEqual(os.readlink(self.lib / "current"), f"releases/{self.b}")
        self.assertEqual(self.names(), {self.a, self.b, self.c})

    @unittest.skipUnless(Path("/run/systemd/system").is_dir(), "systemd")
    def test_the_lock_is_held_while_pruning(self) -> None:
        stub = self.root / "stub"
        stub.mkdir()
        tried = self.root / "tried"
        systemctl = stub / "systemctl"
        systemctl.write_text(f"#!/bin/sh\nflock -n {self.releases} true; echo $? >> {tried}\n")
        systemctl.chmod(0o755)
        self.pruned(path=stub)
        self.assertEqual(self.script("--activate-staged", self.b, path=stub).returncode, 0)
        self.assertEqual(tried.read_text(), "1\n" * 4)

    def test_a_failed_activation_prunes_nothing(self) -> None:
        (self.releases / self.b / "bin" / "tool").write_text("changed")
        self.assertEqual(self.script("--activate-staged", self.b).returncode, 1)
        self.assertEqual(self.names(), {self.a, self.b, self.c})
        self.assertEqual(os.readlink(self.lib / "current"), f"releases/{self.c}")

    def test_an_activation_that_does_not_read_back_fails_and_prunes_nothing(self) -> None:
        module = load_script()
        args = module.parser().parse_args(["--prefix", str(self.prefix), "--activate-staged", self.b])
        path = os.pathsep.join([str(self.prefix / "bin"), os.environ["PATH"]])
        with mock.patch.dict(os.environ, {"PATH": path}), mock.patch.object(
            module, "atomic_symlink", lambda *_: None
        ), self.assertRaisesRegex(module.ReleaseError, "does not reach"):
            module.install(args)
        self.assertEqual(self.names(), {self.a, self.b, self.c})

    def test_waits_for_another_installer(self) -> None:
        descriptor = os.open(self.releases, os.O_RDONLY)
        self.addCleanup(os.close, descriptor)
        fcntl.flock(descriptor, fcntl.LOCK_EX)
        for arguments in (["--prune-only"], ["--activate-staged", self.b]):
            path = os.pathsep.join([str(self.prefix / "bin"), os.environ["PATH"]])
            started = subprocess.Popen(
                [sys.executable, str(SCRIPT), "--prefix", str(self.prefix), *arguments],
                env={**os.environ, "PATH": path}, stdout=subprocess.DEVNULL,
            )
            with self.assertRaises(subprocess.TimeoutExpired):
                started.wait(timeout=2)
            started.kill()
            started.wait()
        self.assertEqual(self.names(), {self.a, self.b, self.c})
        self.assertEqual(os.readlink(self.lib / "current"), f"releases/{self.c}")


if __name__ == "__main__":
    unittest.main()
