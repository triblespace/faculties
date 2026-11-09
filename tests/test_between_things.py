"""The carried predicate's true / false / error contract, without live faculties."""
import contextlib
import io
import json
import os
from pathlib import Path
import runpy
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

SCRIPT = Path(__file__).resolve().parents[1] / "scripts" / "between-things.py"
PERSON = "a" * 32
GOAL = "b" * 32


class PredicateTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.cache = self.root / "colony-coverage" / "tester.json"
        self.pile = str(self.root / "self.pile")
        self.person = f"person: {PERSON}\nlabel: tester\n"
        self.listing = "No goals yet.\n"
        self.show = f"Goal {GOAL}\nTitle: goal\n"
        self.failure = None

    def command(self, argv, **kwargs):
        self.assertEqual(os.environ["PILE"], self.pile)
        if self.failure:
            raise self.failure
        self.assertNotIn("status", Path(argv[0]).name)
        if Path(argv[0]).name == "relations":
            self.assertEqual(argv[1:], ["show", "tester"])
            return subprocess.CompletedProcess(argv, 0, self.person, "")
        return subprocess.CompletedProcess(argv, 0, self.listing if argv[1] == "list" else self.show, "")

    def invoke(self, refresh=True, command=None, replace=None, lock=False, mask=None, extra=(), persona="tester"):
        out, err = io.StringIO(), io.StringIO()
        with contextlib.ExitStack() as stack:
            stack.enter_context(patch.dict(os.environ, {"PILE": self.pile, "XDG_CACHE_HOME": str(self.root)}))
            stack.enter_context(patch.object(sys, "argv", [str(SCRIPT), "--persona", persona] + (["--refresh"] if refresh else []) + (["--lock-tag", "tester"] if lock else []) + list(extra)))
            stack.enter_context(patch("subprocess.run", side_effect=command or self.command))
            spawned = stack.enter_context(patch("subprocess.Popen"))
            if lock:
                stack.enter_context(patch("os.access", return_value=True))
            # Never install timers or handlers in the test runner.
            for name in ("alarm", "signal", "pthread_sigmask"):
                stack.enter_context(patch.object(signal, name, side_effect=mask if name == "pthread_sigmask" else None))
            if replace:
                stack.enter_context(patch("os.replace", side_effect=replace))
            stack.enter_context(contextlib.redirect_stdout(out))
            stack.enter_context(contextlib.redirect_stderr(err))
            with self.assertRaises(SystemExit) as exited:
                runpy.run_path(str(SCRIPT), run_name="__main__")
        return exited.exception.code, out.getvalue(), err.getvalue(), spawned.call_count

    def stored(self):
        return json.loads(self.cache.read_text())

    def test_true_is_preserved_in_cache(self):
        self.assertEqual(self.invoke()[0], 0)
        self.assertEqual(self.stored()["result"], "due")
        code, _, error, spawns = self.invoke(False)
        self.assertEqual((code, error, spawns), (0, "", 0))

    def test_exact_persona_never_calls_status_or_relations(self):
        def command(argv, **kwargs):
            self.assertEqual(Path(argv[0]).name, "compass")
            return self.command(argv, **kwargs)
        self.assertEqual(self.invoke(command=command, persona=PERSON)[0], 0)

    def test_relations_alias_resolves_without_status(self):
        self.assertEqual(self.invoke()[0], 0)
        self.assertEqual(self.stored()["result"], "due")

    def test_relations_ambiguity_is_error_not_idle(self):
        def ambiguous(argv, **kwargs):
            return subprocess.CompletedProcess(argv, 1, "", "ambiguous person selector")
        self.assertEqual(self.invoke(command=ambiguous)[0], 2)
        self.assertIn("ambiguous person selector", self.stored()["why"])

    def test_owned_reservation_still_covers(self):
        def command(argv, **kwargs):
            if argv[1] == "check":
                return subprocess.CompletedProcess(argv, 3, "tag=tester-build", "")
            return self.command(argv, **kwargs)
        self.assertEqual(self.invoke(command=command, lock=True)[0], 1)

    def test_relations_failure_keeps_stderr_through_cache(self):
        def failed(argv, **kwargs):
            return subprocess.CompletedProcess(argv, 2, "", "PILE required")
        code, _, error, _ = self.invoke(command=failed)
        self.assertEqual(code, 2)
        self.assertIn("relations show <persona> exited 2", error)
        self.assertIn("stderr=PILE required", error)
        self.assertEqual(self.stored()["result"], "error")
        self.assertIn("PILE required", self.invoke(False)[2])

    def test_cached_error_diagnostic_stays_identical_as_clock_advances(self):
        self.failure = RuntimeError("relations unavailable")
        self.assertEqual(self.invoke()[0], 2)
        checked = self.stored()["at"]
        with patch("time.time", return_value=checked + 1):
            first = self.invoke(False)
        with patch("time.time", return_value=checked + 61):
            second = self.invoke(False)
        self.assertEqual((first[0], second[0]), (2, 2))
        self.assertEqual(first[2], second[2])
        self.assertEqual(first[2].strip(), self.stored()["why"])
        self.assertEqual((first[3], second[3]), (0, 0))

    def test_call_timeout_is_cached_error(self):
        self.failure = subprocess.TimeoutExpired(["relations", "show"], 120)
        self.assertEqual(self.invoke()[0], 2)
        self.assertIn("TimeoutExpired", self.stored()["why"])
        self.assertEqual(self.invoke(False)[0], 2)

    def test_call_timeout_preserves_bounded_stderr_but_not_stdout_or_argv(self):
        self.failure = subprocess.TimeoutExpired(
            ["private-path", "private-selector"], 120,
            output=b"raw-goal-body", stderr="waiting for reader lock\n".encode() + b"\xff" + "é".encode() * 3000)
        code, out, error, _ = self.invoke()
        self.assertEqual((code, out), (2, ""))
        why = self.stored()["why"]
        self.assertEqual(error.strip(), why)
        for expected in ("TimeoutExpired", "call exceeded 120 s", "phase=wait", "call=1 [relations show <persona>]",
                         "stderr=waiting for reader lock"):
            self.assertIn(expected, why)
        for timing in ("call_elapsed", "refresh_start", "refresh_end", "refresh_elapsed"):
            self.assertNotIn(timing, why)
        self.assertIn("refresh_elapsed", self.stored()["diagnostic"])
        for private in ("raw-goal-body", "private-path", "private-selector"):
            self.assertNotIn(private, why)
        self.assertLessEqual(len(why.encode()), 1800)

    def test_later_whole_alarm_identifies_active_call_and_elapsed_time(self):
        clock = [100.0]
        self.listing = f"== DOING (1) ==\n- [{GOAL}] goal\n"
        def waiting(argv, **kwargs):
            self.assertEqual(kwargs["timeout"], 120)
            if argv[1:] == ["show", GOAL]:
                clock[0] = 580.0
                signal.signal.call_args_list[0].args[1]()
            clock[0] += 3
            return self.command(argv, **kwargs)
        with patch("time.monotonic", side_effect=lambda: clock[0]), patch("time.time", return_value=1000):
            self.assertEqual(self.invoke(command=waiting)[0], 2)
        cached = self.stored()
        why = cached["why"]
        self.assertIn(f"phase=wait call=3 [compass show {GOAL}]", why)
        self.assertEqual(cached["diagnostic"], {
            "refresh_start": 1000, "refresh_end": 1000, "refresh_elapsed": 480,
            "phase": "wait", "call": {"index": 3, "command": f"compass show {GOAL}", "elapsed": 474}})
        self.assertEqual((cached["at"], cached["until"], cached["context"]["version"]), (1000, 1600, 3))
        with patch("time.time", return_value=1100):
            code, _, error, spawns = self.invoke(False)
        self.assertEqual((code, error.strip(), spawns), (2, why, 0))

    def test_nonzero_later_call_retains_only_error_stderr(self):
        clock = [100.0]
        def failed(argv, **kwargs):
            clock[0] += 7
            if argv[1] == "list":
                return subprocess.CompletedProcess(argv, 2, "private-goal-body", "collection unavailable")
            return self.command(argv, **kwargs)
        with patch("time.monotonic", side_effect=lambda: clock[0]):
            self.assertEqual(self.invoke(command=failed)[0], 2)
        why = self.stored()["why"]
        self.assertIn("phase=read-result call=2 [compass list doing blocked]", why)
        self.assertEqual(self.stored()["diagnostic"]["call"]["elapsed"], 7)
        self.assertEqual(self.stored()["diagnostic"]["refresh_elapsed"], 14)
        self.assertIn("stderr=collection unavailable", why)
        self.assertNotIn("private-goal-body", why)

    def test_equivalent_failed_refreshes_keep_attention_identity_with_new_timings(self):
        results = []
        for start, duration in ((1000.0, 7.0), (2000.0, 11.0)):
            wall, clock = [start], [100.0]
            def failed(argv, **kwargs):
                wall[0] += duration
                clock[0] += duration
                return subprocess.CompletedProcess(argv, 2, "private-body", "collection unavailable")
            with patch("time.time", side_effect=lambda: wall[0]), patch("time.monotonic", side_effect=lambda: clock[0]):
                code, _, refresh_error, _ = self.invoke(command=failed)
                cached = self.stored()
                foreground = self.invoke(False)
            self.assertEqual((code, foreground[0], foreground[3]), (2, 2, 0))
            self.assertEqual(refresh_error, foreground[2])
            self.assertEqual(refresh_error.strip(), cached["why"])
            self.assertEqual(cached["diagnostic"]["refresh_start"], start)
            self.assertEqual(cached["diagnostic"]["refresh_end"], start + duration)
            self.assertEqual(cached["diagnostic"]["refresh_elapsed"], duration)
            self.assertEqual(cached["diagnostic"]["call"]["elapsed"], duration)
            results.append((cached, foreground[2]))
        self.assertEqual(results[0][0]["why"], results[1][0]["why"])
        self.assertEqual(results[0][1], results[1][1])
        self.assertNotEqual(results[0][0]["diagnostic"], results[1][0]["diagnostic"])

    def test_old_error_cache_without_diagnostic_remains_readable(self):
        self.failure = RuntimeError("relations unavailable")
        self.assertEqual(self.invoke()[0], 2)
        cached = self.stored()
        cached.pop("diagnostic")
        self.cache.write_text(json.dumps(cached))
        code, _, error, spawns = self.invoke(False)
        self.assertEqual((code, error.strip(), spawns), (2, cached["why"], 0))

    def test_whole_refresh_timeout_is_cached_error(self):
        def timed_out(argv, **kwargs):
            # Invoke the exact handler installed by the script.
            signal.signal.call_args_list[0].args[1]()
        self.assertEqual(self.invoke(command=timed_out)[0], 2)
        self.assertIn("refresh past 480 s", self.stored()["why"])

    def test_timeout_at_publication_transition_is_cached_error(self):
        calls = 0
        def mask(*args):
            nonlocal calls
            calls += 1
            if calls == 1:
                signal.signal.call_args_list[0].args[1]()
        self.assertEqual(self.invoke(mask=mask)[0], 2)
        self.assertIn("refresh past 480 s", self.stored()["why"])
        self.assertIn("phase=publish", self.stored()["why"])

    def test_argument_parse_failure_is_error(self):
        for flag in ("--fresh-mins", "--cache-secs"):
            code, _, error, _ = self.invoke(extra=(flag, "invalid"))
            self.assertEqual(code, 2)
            self.assertIn("invalid numeric argument", error)

    def test_parse_errors_never_become_due_or_covered(self):
        for output in ("", "person: bad\n", f"changed format {PERSON}\n"):
            with self.subTest(output=output):
                self.person = output
                self.assertEqual(self.invoke()[0], 2)
        self.person = f"person: {PERSON}\nlabel: tester\n"
        self.listing = "new or truncated board format"
        self.assertEqual(self.invoke()[0], 2)
        self.listing = f"== DOING (1) ==\n- [{GOAL}] goal\n"
        self.show = "unrecognized goal"
        self.assertEqual(self.invoke()[0], 2)

    def test_nested_goal_activity_still_covers(self):
        self.listing = f"== DOING (1) ==\n  - [{GOAL}] goal\n"
        stamp = time.strftime("%Y-%m-%dT%H:%M:%S", time.gmtime())
        self.show += f"Status history:\n- {stamp} TAI doing by {PERSON}\n"
        self.assertEqual(self.invoke()[0], 1)
        cached = self.stored()
        self.assertEqual(set(cached), {"result", "why", "at", "until", "context"})
        self.assertNotIn("refresh_elapsed", cached["why"])
        self.assertEqual(self.invoke(False)[0], 1)

    def test_current_cache_grace_and_expiry_are_unchanged(self):
        self.assertEqual(self.invoke()[0], 0)
        until = self.stored()["until"]
        for offset, expected_code, expected_spawns in ((-1, 0, 0), (0, 0, 1), (599, 0, 1), (600, 2, 1)):
            with self.subTest(offset=offset), patch("time.time", return_value=until + offset):
                code, _, _, spawns = self.invoke(False)
            self.assertEqual((code, spawns), (expected_code, expected_spawns))

    def test_unavailable_or_unidentified_reservation_is_error(self):
        for code, text in ((4, "UNREACHABLE stars"), (3, ""), (0, "unexpected")):
            with self.subTest(code=code, text=text):
                def checked(argv, **kwargs):
                    if argv[1] == "check":
                        return subprocess.CompletedProcess(argv, code, text, "")
                    return self.command(argv, **kwargs)
                self.assertEqual(self.invoke(command=checked, lock=True)[0], 2)
                self.assertEqual(self.stored()["result"], "error")

    def test_compass_failure_is_error(self):
        def failed(argv, **kwargs):
            if "compass" in Path(argv[0]).name:
                return subprocess.CompletedProcess(argv, 2, "", "collection unavailable")
            return self.command(argv, **kwargs)
        self.assertEqual(self.invoke(command=failed)[0], 2)
        self.assertIn("collection unavailable", self.stored()["why"])

    def test_missing_invalid_legacy_and_wrong_pile_cache_are_errors(self):
        for content in (None, "{", json.dumps({"due": False, "why": "status unreadable"})):
            with self.subTest(content=content):
                if content is not None:
                    self.cache.parent.mkdir(parents=True, exist_ok=True)
                    self.cache.write_text(content)
                code, _, error, spawns = self.invoke(False)
                self.assertEqual((code, spawns), (2, 1))
                self.assertIn("cache unreadable", error)
        self.invoke()
        self.pile = str(self.root / "other.pile")
        self.assertEqual(self.invoke(False)[0], 2)

    def test_expired_result_and_failed_cache_publication_are_errors(self):
        self.invoke()
        cached = self.stored()
        cached["at"] = cached["until"] = 0
        self.cache.write_text(json.dumps(cached))
        self.assertEqual(self.invoke(False)[0], 2)
        code, _, error, _ = self.invoke(replace=OSError("disk full"))
        self.assertEqual(code, 2)
        self.assertIn("cache publication failed: disk full", error)

    def test_version_two_covered_cache_requires_refresh(self):
        self.assertEqual(self.invoke()[0], 0)
        cached = self.stored()
        cached["context"]["version"] = 2
        cached["result"] = "covered"
        cached["why"] = "old authored status says waiting"
        self.cache.write_text(json.dumps(cached))
        code, _, error, spawns = self.invoke(False)
        self.assertEqual((code, spawns), (2, 1))
        self.assertIn("cache context differs", error)


if __name__ == "__main__":
    unittest.main()
