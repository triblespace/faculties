#!/usr/bin/env python3
"""Offline socket-level checks; never connects to a Codex installation."""
import argparse
import base64
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("delivery", Path(__file__).with_name("orient_deliver.py"))
delivery = importlib.util.module_from_spec(spec)
spec.loader.exec_module(delivery)


def read_exact(conn, size):
    data = b""
    while len(data) < size:
        chunk = conn.recv(size - len(data))
        if not chunk:
            raise EOFError()
        data += chunk
    return data


def frame(conn, opcode, data, final=True):
    header = bytes([(128 if final else 0) | opcode])
    if len(data) < 126:
        header += bytes([len(data)])
    elif len(data) < 65536:
        header += b"\x7e" + struct.pack("!H", len(data))
    else:
        header += b"\x7f" + struct.pack("!Q", len(data))
    conn.sendall(header + data)


class Server:
    def __init__(self, path, case="active"):
        self.path, self.case, self.requests, self.error = str(path), case, [], None
        self.listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.listener.bind(self.path)
        self.listener.listen(1)
        self.listener.settimeout(2)
        self.worker = threading.Thread(target=self.run)
        self.worker.start()

    def run(self):
        try:
            with self.listener.accept()[0] as conn:
                conn.settimeout(2)
                header = b""
                while not header.endswith(b"\r\n\r\n"):
                    header += read_exact(conn, 1)
                key = next(line.split(b": ", 1)[1] for line in header.split(b"\r\n")
                           if line.startswith(b"Sec-WebSocket-Key:"))
                accept = base64.b64encode(hashlib.sha1(key + b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11").digest())
                conn.sendall(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: " + accept + b"\r\n\r\n")
                while True:
                    first, second = read_exact(conn, 2)
                    assert second & 128, "client frames must be masked"
                    size = second & 127
                    if size == 126:
                        size = struct.unpack("!H", read_exact(conn, 2))[0]
                    elif size == 127:
                        size = struct.unpack("!Q", read_exact(conn, 8))[0]
                    mask = read_exact(conn, 4)
                    raw = read_exact(conn, size)
                    raw = bytes(b ^ mask[i % 4] for i, b in enumerate(raw))
                    if first & 15 == 10:
                        continue
                    request = json.loads(raw)
                    self.requests.append(request)
                    if "id" not in request or "method" not in request:
                        continue
                    method = request["method"]
                    if method == "turn/start" and self.case == "approval":
                        frame(conn, 1, json.dumps({"id": "approval-1", "method": "item/commandExecution/requestApproval", "params": {}}).encode())
                        # Any response from the adapter would appear in requests.
                        continue
                    result = self.result(method, request)
                    if method == "turn/start" and self.case == "timeout":
                        time.sleep(0.25)
                        return
                    response = {"id": request["id"], "result": result}
                    if method == "turn/start" and self.case in ("rejected", "internal"):
                        response = {"id": request["id"], "error": {
                            "code": -32602 if self.case == "rejected" else -32603, "message": "test rejection"}}
                    if method == "turn/start" and self.case == "wrong_id":
                        response["id"] += 1
                    if method == "turn/start" and self.case == "malformed":
                        raw = b'{"id":4,"result":{},"result":{}}'
                    else:
                        raw = json.dumps(response).encode()
                    if self.case == "frames":
                        frame(conn, 1, json.dumps({"method": "turn/started", "params": {}}).encode())
                        frame(conn, 9, b"ping")
                        frame(conn, 1, raw[:5], False)
                        frame(conn, 0, raw[5:])
                    else:
                        frame(conn, 1, raw)
        except (EOFError, BrokenPipeError, ConnectionResetError):
            pass
        except BaseException as error:
            self.error = error
        finally:
            self.listener.close()

    def result(self, method, request):
        if method == "initialize":
            assert request["params"] == {"clientInfo": {
                "name": "codex-backend", "title": "Orient delivery", "version": "orient-delivery-1"}}, "must not change global server identity"
            user_agent = {
                "wrong_version": "codex/0.159.0",
                "incidental_version": "probe/0.159.0 (client 0.160.1)",
                "alternate_product": "astra-recovery-probe/0.160.1 (Ubuntu; aarch64) unknown (codex-tui; 0.160.1)",
                "product_spaces": "Astra Recovery Probe/0.160.1 (Ubuntu; aarch64)",
                "product_cr": "probe\rspoof/0.160.1",
                "product_lf": "probe\nspoof/0.160.1",
                "version_cr": "probe/0.160.1\r(client 0.160.1)",
                "version_lf": "probe/0.160.1\n(client 0.160.1)",
                "malformed_ua": "client has 0.160.1 but no primary version",
            }.get(self.case, "codex/0.160.1")
            return {"codexHome": "/wrong" if self.case == "wrong_home" else "/test/codex",
                    "userAgent": user_agent}
        if method == "thread/loaded/list":
            if self.case == "pause_during_absence":
                (Path(self.path).parent / "paused").touch()
                return {"data": [], "nextCursor": None}
            if self.case == "absent_paged":
                return {"data": ["other"], "nextCursor": "last" if request["params"]["cursor"] is None else None}
            if self.case == "bad_page":
                return {"data": [42], "nextCursor": None}
            if self.case == "absent_missing_cursor":
                return {"data": []}
            if self.case == "present_missing_cursor":
                return {"data": ["owner"]}
            if self.case == "present_more_pages":
                return {"data": ["owner"], "nextCursor": "unused"}
            if self.case == "present_paged":
                return {"data": ["other"], "nextCursor": "last"} if request["params"]["cursor"] is None else {"data": ["owner"]}
            if self.case == "malformed_cursor":
                return {"data": ["owner"], "nextCursor": 42}
            if self.case == "empty_cursor":
                return {"data": ["owner"], "nextCursor": ""}
            if self.case == "repeat_cursor_with_owner":
                return {"data": [] if request["params"]["cursor"] is None else ["owner"], "nextCursor": "loop"}
            if self.case == "loop_cursor":
                return {"data": [], "nextCursor": "loop"}
            return {"data": [] if self.case == "not_loaded" else ["owner"], "nextCursor": None}
        if method == "thread/read":
            assert request["params"] == {"threadId": "owner", "includeTurns": False}
            return {"thread": {"id": "wrong" if self.case == "wrong_thread" else "owner",
                "status": {"type": "idle" if self.case in ("idle", "busy_transition") else
                           "notLoaded" if self.case == "stale" else
                           "systemError" if self.case == "system_error" else
                           "mystery" if self.case == "unknown_status" else "active"},
                "canAcceptDirectInput": None if self.case == "no_input" else True}}
        if method == "turn/start":
            assert set(request["params"]) == {"threadId", "input"}, "must inherit all settings"
            assert request["params"]["threadId"] == "owner"
            return {"turn": {"id": "raced-active" if self.case == "busy_transition" else "turn-1",
                             "status": "failed" if self.case == "failed_start" else "inProgress", "error": None}}
        raise AssertionError("unexpected method: " + method)

    def finish(self):
        self.worker.join(3)
        if self.worker.is_alive():
            raise AssertionError("fake server stuck")
        if self.error:
            raise self.error
        os.unlink(self.path)


class DeliveryTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="orient-deliver-test-")
        self.root = Path(self.temp.name)
        self.args = argparse.Namespace(thread="owner", socket=str(self.root / "socket"),
            state_dir=str(self.root / "state"), codex_home="/test/codex", server_version="0.160.1",
            pause_marker=[str(self.root / "paused")], probe=False)

    def tearDown(self):
        self.temp.cleanup()

    def state(self):
        paths = list((self.root / "state").glob("*.json"))
        return json.loads(paths[0].read_text()) if paths else None

    def run_case(self, case, fails=False):
        server = Server(self.args.socket, case)
        report = "News: Unicode π\n$(not shell)\n\n"
        try:
            if fails:
                with self.assertRaises((delivery.Failure, OSError, ValueError)):
                    delivery.deliver(self.args, report, time.monotonic() + (0.1 if case == "timeout" else 2))
            else:
                delivery.deliver(self.args, report, time.monotonic() + 2)
        finally:
            server.finish()
        sent = [req for req in server.requests if req.get("method") == "turn/start"]
        for req in sent:
            self.assertEqual(req["params"]["input"], [{"type": "text", "text": report}])
        return sent

    def test_active(self):
        self.assertEqual(len(self.run_case("active")), 1)
        self.assertEqual(self.state()["status"], "accepted")

    def test_idle(self):
        self.assertEqual(len(self.run_case("idle")), 1)

    def test_idle_to_busy_is_one_atomic_start_or_steer(self):
        self.run_case("busy_transition")
        self.assertEqual(self.state()["turn"], "raced-active")

    def test_ping_notifications_and_fragmented_responses(self):
        self.run_case("frames")

    def test_stable_socket_symlink_is_resolved_per_connection(self):
        target = self.root / "actual-socket"
        Path(self.args.socket).symlink_to(target)
        server = Server(target)
        try:
            delivery.deliver(self.args, "report", time.monotonic() + 2)
        finally:
            server.finish()
        self.assertEqual(self.state()["status"], "accepted")

    def test_never_answers_owner_approval(self):
        server = Server(self.args.socket, "approval")
        try:
            with self.assertRaisesRegex(delivery.Failure, "interactive owner"):
                delivery.deliver(self.args, "report", time.monotonic() + 2)
        finally:
            server.finish()
        self.assertFalse(any(req.get("id") == "approval-1" for req in server.requests))
        self.assertEqual(self.state()["status"], "unknown")

    def test_owner_checks_before_dispatch(self):
        for case in ("wrong_home", "wrong_version", "not_loaded", "stale", "unknown_status", "wrong_thread", "no_input"):
            with self.subTest(case=case):
                self.assertEqual(self.run_case(case, True), [])
                self.assertIsNone(self.state())

    def test_probe_has_no_dispatch_or_state(self):
        self.args.probe = True
        self.assertEqual(self.run_case("active"), [])
        self.assertFalse((self.root / "state").exists())

    def test_rejected_is_not_success_and_can_be_retried_deliberately(self):
        self.assertEqual(len(self.run_case("rejected", True)), 1)
        self.assertEqual(self.state()["status"], "rejected")
        self.run_case("active")

    def test_ambiguous_response_blocks_retry_before_socket(self):
        for case in ("timeout", "internal", "wrong_id", "malformed", "failed_start"):
            with self.subTest(case=case):
                # Each case gets an independent private state directory.
                self.args.state_dir = str(self.root / case)
                self.assertEqual(len(self.run_case(case, True)), 1)
                state_path = next(Path(self.args.state_dir).glob("*.json"))
                self.assertEqual(json.loads(state_path.read_text())["status"], "unknown")
                with patch.object(delivery, "WebSocket", side_effect=AssertionError("must not reconnect")):
                    with self.assertRaisesRegex(delivery.Failure, "unresolved"):
                        delivery.deliver(self.args, "different body too", time.monotonic() + 2)

    def test_identical_text_can_recur_after_confirmed_acceptance(self):
        self.run_case("active")
        previous = self.state()["attempt"]
        self.run_case("active")
        self.assertNotEqual(previous, self.state()["attempt"])

    def test_pause_prevents_connection(self):
        Path(self.args.pause_marker[0]).touch()
        with patch.object(delivery, "WebSocket", side_effect=AssertionError("must not connect")):
            with self.assertRaisesRegex(delivery.Failure, "paused"):
                delivery.deliver(self.args, "report", time.monotonic() + 2)

    def test_private_state_required(self):
        Path(self.args.state_dir).mkdir(mode=0o755)
        Path(self.args.state_dir).chmod(0o755)
        with self.assertRaisesRegex(delivery.Failure, "private"):
            delivery.deliver(self.args, "report", time.monotonic() + 2)

    def test_state_never_contains_report_body(self):
        self.run_case("active")
        self.assertNotIn("not shell", json.dumps(self.state()))
        self.assertRegex(self.state()["sha256"], r"^[0-9a-f]{64}$")

    def test_active_to_idle_is_also_one_atomic_operation(self):
        # Same response shape whether Core starts after the active turn ended,
        # or steers the observed turn. The adapter neither guesses nor retries.
        self.assertEqual(len(self.run_case("active_to_idle")), 1)

    def test_exit_status_classification_and_context_are_explicit(self):
        for error, code in [
            (delivery.KnownNotSubmitted("known unsent"), 75),
            (delivery.Paused("operator pause"), 78),
            (delivery.CompatibilityFailure("version"), 78),
            (delivery.Failure("unknown dispatch"), 1),
            (TimeoutError("timeout"), 1),
            (BrokenPipeError("partial stdin"), 1),
            (delivery.Rejected("turn/start", -32602), 1),
        ]:
            self.assertEqual(delivery.exit_status(error), code)

    def test_readiness_failures_are_typed_without_submission(self):
        for case, expected in [
            ("not_loaded", 75), ("absent_paged", 75),
            ("stale", 75), ("system_error", 75), ("wrong_version", 78),
            ("incidental_version", 78), ("malformed_ua", 1),
            ("product_cr", 1), ("product_lf", 1), ("version_cr", 1), ("version_lf", 1),
            ("wrong_home", 1), ("wrong_thread", 1), ("unknown_status", 1),
            ("no_input", 1), ("bad_page", 1), ("absent_missing_cursor", 75),
            ("loop_cursor", 1), ("malformed_cursor", 1), ("empty_cursor", 1),
            ("repeat_cursor_with_owner", 1),
        ]:
            with self.subTest(case=case):
                server = Server(self.args.socket, case)
                try:
                    with self.assertRaises(delivery.Failure) as caught:
                        delivery.deliver(self.args, "fixed", time.monotonic() + 2)
                    self.assertEqual(delivery.exit_status(caught.exception), expected)
                finally:
                    server.finish()
                self.assertFalse(any(req.get("method") == "turn/start" for req in server.requests))
                self.assertIsNone(self.state())

    def test_optional_cursor_present_owner_and_pagination(self):
        for case, pages in [("present_missing_cursor", 1), ("present_more_pages", 1),
                            ("present_paged", 2), ("active", 1)]:
            with self.subTest(case=case):
                server = Server(self.args.socket, case)
                try:
                    delivery.deliver(self.args, "fixed", time.monotonic() + 2)
                finally:
                    server.finish()
                self.assertEqual(sum(req.get("method") == "thread/loaded/list" for req in server.requests), pages)
                self.assertEqual(sum(req.get("method") == "turn/start" for req in server.requests), 1)
                self.assertEqual(self.state()["status"], "accepted")

    def test_primary_product_may_contain_spaces(self):
        self.assertEqual(len(self.run_case("product_spaces")), 1)
        self.assertEqual(self.state()["status"], "accepted")

    def native(self):
        return subprocess.run(
            [sys.executable, str(Path(__file__).with_name("orient_deliver.py")),
             self.args.thread, "--socket", self.args.socket,
             "--state-dir", self.args.state_dir, "--codex-home", self.args.codex_home,
             "--server-version", self.args.server_version,
             "--pause-marker", self.args.pause_marker[0], "--deadline", "1"],
            input="fixed π\n", text=True, capture_output=True, timeout=4,
        )

    def test_native_cli_exit_status_from_fixed_fake_socket(self):
        for case, expected in [
            ("active", 0), ("not_loaded", 75), ("absent_paged", 75),
            ("alternate_product", 0), ("incidental_version", 78), ("malformed_ua", 1),
            ("product_spaces", 0), ("product_cr", 1), ("product_lf", 1),
            ("version_cr", 1), ("version_lf", 1),
            ("stale", 75), ("system_error", 75), ("wrong_version", 78),
            ("wrong_home", 1), ("wrong_thread", 1), ("unknown_status", 1),
            ("bad_page", 1), ("absent_missing_cursor", 75), ("loop_cursor", 1),
            ("present_missing_cursor", 0), ("present_more_pages", 0), ("present_paged", 0),
            ("malformed_cursor", 1), ("empty_cursor", 1), ("repeat_cursor_with_owner", 1),
            ("no_input", 1), ("internal", 1), ("failed_start", 1),
        ]:
            with self.subTest(case=case):
                self.args.state_dir = str(self.root / ("native-" + case))
                server = Server(self.args.socket, case)
                try:
                    result = self.native()
                    self.assertEqual(result.returncode, expected, result.stderr)
                finally:
                    server.finish()
                if expected == 75:
                    self.assertFalse(any(req.get("method") == "turn/start" for req in server.requests))
                    self.assertIsNone(self.state())

    def test_native_pause_socket_and_unresolved_state_never_retry(self):
        Path(self.args.pause_marker[0]).touch()
        self.assertEqual(self.native().returncode, 78)
        Path(self.args.pause_marker[0]).unlink()
        self.assertEqual(self.native().returncode, 1)
        directory = Path(self.args.state_dir)
        directory.mkdir(mode=0o700, exist_ok=True)
        path = directory / (hashlib.sha256(self.args.thread.encode()).hexdigest() + ".json")
        for status in ("unknown", "malformed"):
            path.write_text(json.dumps({"thread": self.args.thread, "status": status}))
            result = self.native()
            self.assertEqual(result.returncode, 1)
            self.assertIn("unresolved dispatch", result.stderr)

    def test_pause_during_preflight_wins_over_known_unsent(self):
        server = Server(self.args.socket, "pause_during_absence")
        try:
            result = self.native()
            self.assertEqual(result.returncode, 78, result.stderr)
        finally:
            server.finish()
        self.assertFalse(any(req.get("method") == "turn/start" for req in server.requests))
        self.assertIsNone(self.state())

    def test_unknown_guard_precedes_every_retryable_readiness_case(self):
        directory = Path(self.args.state_dir)
        directory.mkdir(mode=0o700)
        path = directory / (hashlib.sha256(self.args.thread.encode()).hexdigest() + ".json")
        path.write_text(json.dumps({"thread": self.args.thread, "status": "unknown"}))
        with patch.object(delivery, "check_owner", side_effect=delivery.KnownNotSubmitted("not loaded")) as check:
            with patch.object(delivery, "WebSocket", side_effect=AssertionError("must not connect")):
                with self.assertRaises(delivery.Failure) as caught:
                    delivery.deliver(self.args, "fixed", time.monotonic() + 1)
        self.assertEqual(delivery.exit_status(caught.exception), 1)
        check.assert_not_called()


if __name__ == "__main__":
    unittest.main()
