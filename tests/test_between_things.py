"""Typed predicate decisions and diagnostics, using only injected fake effects.

Native child/crash and filesystem evidence is retained separately; these tests
exercise the real Producer and refresh_worker logic without native I/O, timers,
flocks or child processes. Both true and false use successful JSON envelopes.
"""
import contextlib
import copy
import importlib.util
import io
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import time
from types import SimpleNamespace
import unittest
from unittest.mock import Mock, patch

SCRIPT = Path(__file__).resolve().parents[1] / "scripts" / "between-things.py"
PERSON = "a" * 32


class ForbiddenNativeEffect(BaseException):
    """Must escape the producer's ordinary error-to-verdict conversion."""


class FakeIO:
    """One fixture's cache and ownership; no filesystem or spawned process."""
    def __init__(self):
        self.cache = "/synthetic/coverage.json"
        self.state = None
        self.parent = False
        self.child = False
        self.spawns = 0
        self.failures = {}
        self.publication_error = None

    def acquire(self):
        if self.parent or self.child:
            return None
        self.parent = True
        return 17

    def close(self, fd):
        assert fd == 17 and self.parent
        self.parent = False

    def read(self):
        if self.state is None:
            raise FileNotFoundError
        if isinstance(self.state, Exception):
            raise self.state
        return copy.deepcopy(self.state)

    def write(self, state, durable=False, before_replace=None):
        if not durable and self.publication_error:
            raise self.publication_error
        if before_replace:
            before_replace()
        self.state = copy.deepcopy(state)

    def spawn(self, fd, token):
        assert fd == 17 and self.parent and not self.child
        assert self.state["attempt"]["id"] == token
        self.child = True
        self.spawns += 1

    def read_failure(self, token):
        return self.failures.get(token)

    def observe_failure(self, token, why):
        return self.failures.setdefault(token, why)


class PredicateTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.pile = str(self.root / "self.pile")
        self.person = f"person: {PERSON}\nlabel: tester\n"
        self.activity = "false\n"
        self.commands = []
        self.failure = None
        self.io = FakeIO()
        self.sequence = 0
        spec = importlib.util.spec_from_file_location("between_things_fixture", SCRIPT)
        self.module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.module)  # Import is inert, never __main__.
        self.stack = contextlib.ExitStack()
        self.addCleanup(self.stack.close)
        for name, value in (("time", 1000.0), ("monotonic", 100.0)):
            self.stack.enter_context(patch.object(time, name, return_value=value))
        # Clear collection routes/credentials rather than inheriting live HOME.
        self.stack.enter_context(patch.dict(os.environ, {
            "HOME": str(self.root), "XDG_CACHE_HOME": str(self.root / "cache"),
            "XDG_CONFIG_HOME": str(self.root / "config"),
            "XDG_STATE_HOME": str(self.root / "state"), "TMPDIR": str(self.root),
            "PILE": self.pile, "PATH": "",
        }, clear=True))
        for name in ("NativeIO", "real_clock"):
            self.stack.enter_context(patch.object(
                self.module, name, side_effect=ForbiddenNativeEffect("native effect forbidden")))
        # Block native process creation even if a fixture accidentally escapes
        # the module-specific subprocess proxy below.
        for name in ("run", "Popen"):
            self.stack.enter_context(patch.object(
                subprocess, name, side_effect=ForbiddenNativeEffect("native process forbidden")))

    def command(self, argv, **kwargs):
        self.commands.append(argv)
        self.assertEqual(os.environ["PILE"], self.pile)
        self.assertTrue(kwargs["close_fds"])
        if self.failure:
            raise self.failure
        self.assertNotIn("status", Path(argv[0]).name)
        if Path(argv[0]).name == "relations":
            self.assertEqual(argv[1:], ["show", "tester"])
            return subprocess.CompletedProcess(argv, 0, self.person, "")
        self.assertEqual(Path(argv[0]).name, "compass")
        self.assertEqual(argv[1:4], ["has-activity", "--actor", PERSON])
        self.assertEqual(argv[4], "--since")
        self.assertEqual(argv[6:], ["--status", "doing", "--status", "blocked"])
        return subprocess.CompletedProcess(argv, 0, self.activity, "")

    def context(self, lock=False):
        return {"version": 3, "pile": self.pile, "fresh": 2700,
                "lock_tag": "tester" if lock else None}

    def producer(self, lock=False):
        def token():
            self.sequence += 1
            return "%032x" % self.sequence
        return self.module.Producer(
            self.io, self.context(lock), 600,
            clock=lambda: (time.time(), time.monotonic(), "fixture-boot"),
            token=token)

    def invoke(self, refresh=True, command=None, replace=None, lock=False,
               mask=None, persona="tester"):
        # refresh=True starts an independent synthetic attempt. Poll-only calls
        # retain its state; the explicit retry tests never reset that state.
        if refresh:
            self.io = FakeIO()
        producer = self.producer(lock)
        before = self.io.spawns
        self.module.persona, self.module.fresh = persona, 2700
        self.module.lock_tag, self.module.pile = ("tester" if lock else None), self.pile
        os.environ["PILE"] = self.pile
        if not refresh:
            outcome = producer.poll()
        else:
            self.assertEqual(producer.poll()[0], "deferred")
            token = self.io.state["attempt"]["id"]
            self.io.publication_error = replace
            inode = SimpleNamespace(st_dev=1, st_ino=2)

            def close_child(fd):
                self.assertEqual(fd, 17)
                self.assertTrue(self.io.child)
                self.io.child = False

            self.signals = SimpleNamespace(
                SIGALRM=signal.SIGALRM, SIG_IGN=signal.SIG_IGN,
                SIG_BLOCK=signal.SIG_BLOCK, SIG_UNBLOCK=signal.SIG_UNBLOCK,
                ITIMER_REAL=signal.ITIMER_REAL, signal=Mock(),
                setitimer=Mock(), pthread_sigmask=Mock(side_effect=mask))
            fake_os = SimpleNamespace(
                stat=lambda path: inode, fstat=lambda fd: inode,
                set_inheritable=Mock(), close=close_child,
                path=os.path, access=lambda path, mode: lock, X_OK=os.X_OK)
            with contextlib.ExitStack() as effects:
                for name, replacement in (
                    ("os", fake_os), ("signal", self.signals),
                    ("fcntl", SimpleNamespace(flock=Mock(), LOCK_EX=2, LOCK_NB=4)),
                    ("shutil", SimpleNamespace(which=lambda name: "/inert/" + name)),
                    ("subprocess", SimpleNamespace(
                        run=command or self.command, TimeoutExpired=subprocess.TimeoutExpired,
                        DEVNULL=subprocess.DEVNULL)),
                ):
                    effects.enter_context(patch.object(self.module, name, replacement))
                outcome = self.module.refresh_worker(producer, 17, token)
            self.assertFalse(self.io.child)
        code, out, err = self.module.envelope(outcome)
        return code, out, err, self.io.spawns - before

    def verdict(self):
        return self.io.read()["verdict"]

    def assert_result(self, invocation, result):
        code, out, error, _ = invocation
        self.assertEqual((code, error), (0, ""))
        self.assertEqual(json.loads(out), {"version": 1, "result": result})

    def cli(self, *args):
        # All cases must reject before constructing native I/O or polling.
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = self.module.main(list(args))
        self.module.NativeIO.assert_not_called()
        self.module.real_clock.assert_not_called()
        return code, out.getvalue(), err.getvalue()

    def test_true_is_preserved_in_cache(self):
        self.assertEqual(self.invoke()[0], 0)
        self.assertEqual(self.verdict()["result"], "due")
        code, _, error, spawns = self.invoke(False)
        self.assertEqual((code, error, spawns), (0, "", 0))

    def test_exact_persona_never_calls_status_or_relations(self):
        def command(argv, **kwargs):
            self.assertEqual(Path(argv[0]).name, "compass")
            return self.command(argv, **kwargs)
        self.assertEqual(self.invoke(command=command, persona=PERSON)[0], 0)

    def test_relations_alias_resolves_without_status(self):
        self.assertEqual(self.invoke()[0], 0)
        self.assertEqual(self.verdict()["result"], "due")

    def test_relations_ambiguity_is_error_not_idle(self):
        def ambiguous(argv, **kwargs):
            return subprocess.CompletedProcess(argv, 1, "", "ambiguous person selector")
        self.assertEqual(self.invoke(command=ambiguous)[0], 2)
        self.assertIn("ambiguous person selector", self.verdict()["why"])

    def test_owned_reservation_still_covers(self):
        def command(argv, **kwargs):
            if argv[1] == "check":
                return subprocess.CompletedProcess(argv, 3, "tag=tester-build", "")
            return self.command(argv, **kwargs)
        self.assert_result(self.invoke(command=command, lock=True), "false")

    def test_relations_failure_keeps_stderr_through_cache(self):
        def failed(argv, **kwargs):
            return subprocess.CompletedProcess(argv, 2, "", "PILE required")
        code, _, error, _ = self.invoke(command=failed)
        self.assertEqual(code, 2)
        self.assertIn("relations show <persona> exited 2", error)
        self.assertIn("stderr=PILE required", error)
        self.assertEqual(self.verdict()["result"], "error")
        self.assertIn("PILE required", self.invoke(False)[2])

    def test_cached_error_diagnostic_stays_identical_as_clock_advances(self):
        self.failure = RuntimeError("relations unavailable")
        self.assertEqual(self.invoke()[0], 2)
        checked = self.verdict()["at"]
        with patch("time.time", return_value=checked + 1):
            first = self.invoke(False)
        with patch("time.time", return_value=checked + 61):
            second = self.invoke(False)
        self.assertEqual((first[0], second[0]), (2, 2))
        self.assertEqual(first[2], second[2])
        self.assertEqual(first[2].strip(), self.verdict()["why"])
        self.assertEqual((first[3], second[3]), (0, 0))

    def test_call_timeout_is_cached_error(self):
        self.failure = subprocess.TimeoutExpired(["relations", "show"], 120)
        self.assertEqual(self.invoke()[0], 2)
        self.assertIn("TimeoutExpired", self.verdict()["why"])
        self.assertEqual(self.invoke(False)[0], 2)

    def test_call_timeout_preserves_bounded_stderr_but_not_stdout_or_argv(self):
        self.failure = subprocess.TimeoutExpired(
            ["private-path", "private-selector"], 120,
            output=b"raw-goal-body", stderr="waiting for reader lock\n".encode() + b"\xff" + "é".encode() * 3000)
        code, out, error, _ = self.invoke()
        self.assertEqual((code, out), (2, ""))
        why = self.verdict()["why"]
        self.assertEqual(error.strip(), why)
        for expected in ("TimeoutExpired", "call exceeded 120 s", "phase=wait", "call=1 [relations show <persona>]",
                         "stderr=waiting for reader lock"):
            self.assertIn(expected, why)
        for timing in ("call_elapsed", "refresh_start", "refresh_end", "refresh_elapsed"):
            self.assertNotIn(timing, why)
        self.assertIn("refresh_elapsed", self.verdict()["diagnostic"])
        for private in ("raw-goal-body", "private-path", "private-selector"):
            self.assertNotIn(private, why)
        self.assertLessEqual(len(why.encode()), 1800)

    def test_later_whole_alarm_identifies_active_call_and_elapsed_time(self):
        clock = [100.0]
        def waiting(argv, **kwargs):
            self.assertEqual(kwargs["timeout"], 120)
            if argv[1] == "has-activity":
                clock[0] = 580.0
                self.signals.signal.call_args_list[0].args[1]()
            clock[0] += 3
            return self.command(argv, **kwargs)
        with patch("time.monotonic", side_effect=lambda: clock[0]), patch("time.time", return_value=1000):
            self.assertEqual(self.invoke(command=waiting)[0], 2)
        cached = self.verdict()
        why = cached["why"]
        self.assertIn("phase=wait call=2 [compass has-activity <actor> <since> doing blocked]", why)
        self.assertEqual(cached["diagnostic"], {
            "refresh_start": 1000, "refresh_end": 1000, "refresh_elapsed": 480,
            "phase": "wait", "call": {"index": 2, "command": "compass has-activity <actor> <since> doing blocked", "elapsed": 477}})
        self.assertEqual((cached["at"], cached["until"], self.io.state["context"]["version"]), (1000, 1600, 3))
        with patch("time.time", return_value=1100):
            code, _, error, spawns = self.invoke(False)
        self.assertEqual((code, error.strip(), spawns), (2, why, 0))

    def test_nonzero_later_call_retains_only_error_stderr(self):
        clock = [100.0]
        def failed(argv, **kwargs):
            clock[0] += 7
            if argv[1] == "has-activity":
                return subprocess.CompletedProcess(argv, 2, "private-goal-body", "collection unavailable")
            return self.command(argv, **kwargs)
        with patch("time.monotonic", side_effect=lambda: clock[0]):
            self.assertEqual(self.invoke(command=failed)[0], 2)
        why = self.verdict()["why"]
        self.assertIn("phase=read-result call=2 [compass has-activity <actor> <since> doing blocked]", why)
        self.assertEqual(self.verdict()["diagnostic"]["call"]["elapsed"], 7)
        self.assertEqual(self.verdict()["diagnostic"]["refresh_elapsed"], 14)
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
                cached = self.verdict()
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
        legacy = dict(self.verdict(), context=self.context())
        legacy.pop("diagnostic")
        self.io.state = legacy
        code, _, error, spawns = self.invoke(False)
        self.assertEqual((code, error.strip(), spawns), (2, legacy["why"], 0))
        self.assertEqual(self.io.state, legacy)  # Read-only legacy conversion.

    def test_whole_refresh_timeout_is_cached_error(self):
        def timed_out(argv, **kwargs):
            # Invoke the exact handler installed by the script.
            self.signals.signal.call_args_list[0].args[1]()
        self.assertEqual(self.invoke(command=timed_out)[0], 2)
        self.assertIn("refresh exceeded fixed attempt deadline", self.verdict()["why"])

    def test_timeout_at_publication_transition_is_cached_error(self):
        calls = 0
        def mask(*args):
            nonlocal calls
            calls += 1
            if calls == 1:
                self.signals.signal.call_args_list[0].args[1]()
        self.assertEqual(self.invoke(mask=mask)[0], 2)
        self.assertIn("refresh exceeded fixed attempt deadline", self.verdict()["why"])
        self.assertIn("phase=publish", self.verdict()["why"])

    def test_argument_parse_failure_is_error(self):
        for flag in ("--fresh-mins", "--cache-secs"):
            for value in ("invalid", "0", "-1"):
                with self.subTest(flag=flag, value=value):
                    code, out, error = self.cli("--result-v1", "--persona", "tester", flag, value)
                    self.assertEqual((code, out), (2, ""))
                    self.assertTrue(error)
        self.assertIn("requires --result-v1", self.cli("--persona", "tester")[2])
        self.assertIn("safe cache basename", self.cli("--result-v1", "--persona", "../unsafe")[2])

    def test_parse_errors_never_become_due_or_covered(self):
        for output in ("", "person: bad\n", f"changed format {PERSON}\n"):
            with self.subTest(output=output):
                self.person = output
                self.assertEqual(self.invoke()[0], 2)
        self.person = f"person: {PERSON}\nlabel: tester\n"
        for output in ("", "new format", "{}", "[]", "null", "1", '"true"', "true false"):
            with self.subTest(output=output):
                self.activity = output
                self.assertEqual(self.invoke()[0], 2)

    def test_native_activity_covers_without_rendering_goal_history(self):
        self.activity = "true\n"
        self.assert_result(self.invoke(), "false")
        cached = self.verdict()
        self.assertEqual(set(cached), {"result", "why", "at", "until"})
        self.assertNotIn("refresh_elapsed", cached["why"])
        self.assert_result(self.invoke(False), "false")

    def test_one_native_query_independent_of_history_size(self):
        # Only a boolean crosses the command boundary, never board/history.
        for activity, result in (("true", "false"), ("false", "true")):
            with self.subTest(activity=activity):
                self.commands.clear()
                self.activity = activity
                self.assert_result(self.invoke(persona=PERSON), result)
                self.assertEqual(len(self.commands), 1)
                self.assertEqual(self.commands[0][1], "has-activity")

    def test_native_cutoff_preserves_whole_tai_calendar_second_boundary(self):
        for now, expected in ((4000.0, "1970-01-01T00:21:40 TAI"),
                              (4000.25, "1970-01-01T00:21:41 TAI")):
            with self.subTest(now=now), patch("time.time", return_value=now):
                self.commands.clear()
                self.assertEqual(self.invoke(persona=PERSON)[0], 0)
                self.assertEqual(self.commands[0][5], expected)

    def test_changing_native_cutoff_does_not_change_failure_identity(self):
        errors = []
        cutoffs = []
        def failed(argv, **kwargs):
            cutoffs.append(argv[5])
            return subprocess.CompletedProcess(argv, 2, "private-body", "collection unavailable")
        for now in (4000.0, 5000.0):
            with patch("time.time", return_value=now):
                self.assertEqual(self.invoke(command=failed, persona=PERSON)[0], 2)
                errors.append(self.verdict()["why"])
        self.assertNotEqual(*cutoffs)
        self.assertEqual(*errors)
        self.assertNotIn("private-body", errors[0])

    def test_expiry_is_fixed_deferred_without_stale_grace(self):
        self.assert_result(self.invoke(), "true")
        until = self.verdict()["until"]
        with patch("time.time", return_value=until - 1):
            self.assert_result(self.invoke(False), "true")
        with patch("time.time", return_value=until):
            first = self.invoke(False)
        self.assertEqual((first[0], first[2], first[3]), (0, "", 1))
        pending = json.loads(first[1])
        self.assertEqual(pending["result"], "deferred")
        self.assertEqual(pending["expires_at"] - pending["started_at"], 480)
        with patch("time.time", return_value=until + 61):
            second = self.invoke(False)
        self.assertEqual((second[0], second[2], second[3]), (0, "", 0))
        self.assertEqual(json.loads(second[1]), pending)
        with patch("time.time", return_value=pending["expires_at"]):
            expired = self.invoke(False)
        self.assertEqual((expired[0], expired[1], expired[3]), (2, "", 0))
        self.assertIn("exceeded fixed deadline", expired[2])
        # A recovered wall clock cannot erase an observed failure.
        with patch("time.time", return_value=until + 62):
            self.assertEqual(self.invoke(False)[2], expired[2])
        self.io.child = False  # Fake owner exits without publication.
        with patch("time.time", return_value=until + 63):
            retry = self.invoke(False)
            self.assertEqual((retry[0], retry[2], retry[3]), (2, expired[2], 1))
            self.assertNotEqual(self.io.state["attempt"]["id"], pending["attempt"])
            self.assertEqual(self.producer().finish(
                self.io.state["attempt"]["id"], "covered", "recovered")[0], "covered")
            self.assert_result(self.invoke(False), "false")

    def test_unavailable_or_unidentified_reservation_is_error(self):
        for code, text in ((4, "UNREACHABLE stars"), (3, ""), (0, "unexpected")):
            with self.subTest(code=code, text=text):
                def checked(argv, **kwargs):
                    if argv[1] == "check":
                        return subprocess.CompletedProcess(argv, code, text, "")
                    return self.command(argv, **kwargs)
                self.assertEqual(self.invoke(command=checked, lock=True)[0], 2)
                self.assertEqual(self.verdict()["result"], "error")

    def test_compass_failure_is_error(self):
        def failed(argv, **kwargs):
            if "compass" in Path(argv[0]).name:
                return subprocess.CompletedProcess(argv, 2, "", "collection unavailable")
            return self.command(argv, **kwargs)
        self.assertEqual(self.invoke(command=failed)[0], 2)
        self.assertIn("collection unavailable", self.verdict()["why"])

    def test_missing_cache_defers_but_invalid_and_wrong_pile_stay_errors(self):
        missing = self.invoke(False)
        self.assertEqual((missing[0], missing[2], missing[3]), (0, "", 1))
        self.assertEqual(json.loads(missing[1])["result"], "deferred")
        for content in (ValueError("malformed JSON"), {"due": False, "why": "status unreadable"}):
            with self.subTest(content=content):
                self.io = FakeIO()
                self.io.state = content
                code, _, error, spawns = self.invoke(False)
                self.assertEqual((code, spawns), (2, 1))
                self.assertIn("cache unreadable", error)
        self.invoke()
        self.pile = str(self.root / "other.pile")
        self.assertEqual(self.invoke(False)[0], 2)

    def test_expired_result_defers_but_failed_publication_remains_error(self):
        self.invoke()
        self.io.state["verdict"]["at"] = self.io.state["verdict"]["until"] = 0
        pending = self.invoke(False)
        self.assertEqual(pending[0], 0)
        self.assertEqual(json.loads(pending[1])["result"], "deferred")
        code, _, error, _ = self.invoke(replace=OSError("disk full"))
        self.assertEqual(code, 2)
        self.assertIn("cache publication failed: disk full", error)
        self.assertIsNone(self.io.state["verdict"])
        # Failed publication is visible even while the automatic retry runs.
        retry = self.invoke(False)
        self.assertEqual((retry[0], retry[2], retry[3]), (2, error, 1))

    def test_version_two_covered_cache_requires_refresh(self):
        self.assertEqual(self.invoke()[0], 0)
        legacy = dict(self.verdict(), context=dict(self.context(), version=2),
                      result="covered", why="old authored status says waiting")
        self.io.state = legacy
        code, _, error, spawns = self.invoke(False)
        self.assertEqual((code, spawns), (2, 1))
        self.assertIn("cache context differs", error)


if __name__ == "__main__":
    unittest.main()
