#!/usr/bin/env python3
"""One Orient callback -> the existing owner's atomic start-or-steer API.

No server startup, thread resume, queue, settings overrides, or automatic retry.
An uncertain write blocks subsequent deliveries until operator reconciliation.
"""
import argparse
import base64
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import select
import socket
import stat
import struct
import sys
import tempfile
import time

MAX_REPORT = 2 * 1024 * 1024
MAX_FRAME = 8 * 1024 * 1024


class Failure(Exception):
    pass


class KnownNotSubmitted(Failure):
    """Verified owner is temporarily unready; no turn/start was attempted."""


class CompatibilityFailure(Failure):
    """A protocol/identity version needs independent attention, never retry."""


class Paused(Failure):
    pass


def exit_status(error):
    if isinstance(error, KnownNotSubmitted):
        return 75
    if isinstance(error, (CompatibilityFailure, Paused)):
        return 78
    return 1


class Rejected(Failure):
    def __init__(self, method, code):
        super().__init__(f"{method} rejected (RPC {code})")
        self.code = code


def remaining(deadline):
    left = deadline - time.monotonic()
    if left <= 0:
        raise Failure("deadline exceeded")
    return left


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise Failure("duplicate JSON field")
        result[key] = value
    return result


def decode_json(data):
    return json.loads(data, object_pairs_hook=unique_object,
                      parse_constant=lambda _: (_ for _ in ()).throw(Failure("invalid JSON constant")))


class WebSocket:
    """Minimal RFC6455 text client over an explicitly configured local UDS."""
    def __init__(self, path, deadline):
        # macOS managed daemons expose a stable symlink to a private /tmp socket.
        # Resolve afresh for every call; never pin that ephemeral target in config.
        path = str(Path(path).resolve(strict=True))
        info = os.stat(path, follow_symlinks=False)
        if not stat.S_ISSOCK(info.st_mode) or info.st_uid != os.getuid():
            raise Failure("socket must be owned by the current user")
        self.deadline = deadline
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            self.sock.settimeout(remaining(deadline))
            self.sock.connect(path)
            key = base64.b64encode(os.urandom(16)).decode()
            self.write(("GET / HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\n"
                        "Connection: Upgrade\r\nSec-WebSocket-Version: 13\r\n"
                        f"Sec-WebSocket-Key: {key}\r\n\r\n").encode())
            header = bytearray()
            while not header.endswith(b"\r\n\r\n"):
                if len(header) >= 16384:
                    raise Failure("oversized WebSocket handshake")
                header.extend(self.read(1))
            lines = header.decode("ascii").split("\r\n")
            fields = {}
            for line in lines[1:-2]:
                name, value = line.split(":", 1)
                name = name.lower()
                if name in fields:
                    raise Failure("duplicate handshake field")
                fields[name] = value.strip()
            accept = base64.b64encode(hashlib.sha1(
                (key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest()).decode()
            if (lines[0].split()[:2] != ["HTTP/1.1", "101"]
                    or fields.get("upgrade", "").lower() != "websocket"
                    or "upgrade" not in fields.get("connection", "").lower().split(", ")
                    or fields.get("sec-websocket-accept") != accept
                    or "sec-websocket-extensions" in fields):
                raise Failure("invalid WebSocket handshake")
        except BaseException:
            self.close()
            raise

    def close(self):
        self.sock.close()

    def read(self, size):
        data = bytearray()
        while len(data) < size:
            self.sock.settimeout(remaining(self.deadline))
            chunk = self.sock.recv(size - len(data))
            if not chunk:
                raise Failure("connection closed")
            data.extend(chunk)
        return bytes(data)

    def write(self, data):
        self.sock.settimeout(remaining(self.deadline))
        self.sock.sendall(data)

    def send_frame(self, opcode, data):
        mask = os.urandom(4)
        size = len(data)
        header = bytes([0x80 | opcode])
        if size < 126:
            header += bytes([0x80 | size])
        elif size <= 65535:
            header += bytes([0xFE]) + struct.pack("!H", size)
        else:
            header += bytes([0xFF]) + struct.pack("!Q", size)
        self.write(header + mask + bytes(value ^ mask[i % 4] for i, value in enumerate(data)))

    def send(self, value):
        self.send_frame(1, json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode())

    def receive(self):
        message = bytearray()
        fragmented = False
        while True:
            first, second = self.read(2)
            final, opcode = bool(first & 0x80), first & 15
            if first & 0x70 or second & 0x80:
                raise Failure("invalid server frame flags")
            size = second & 127
            if size == 126:
                size = struct.unpack("!H", self.read(2))[0]
            elif size == 127:
                size = struct.unpack("!Q", self.read(8))[0]
            if size > MAX_FRAME or len(message) + size > MAX_FRAME:
                raise Failure("oversized server message")
            if opcode >= 8 and (not final or size > 125):
                raise Failure("invalid control frame")
            data = self.read(size)
            if opcode == 8:
                raise Failure("server closed WebSocket")
            if opcode == 9:
                self.send_frame(10, data)
                continue
            if opcode == 10:
                continue
            if (opcode == 1 and fragmented) or (opcode == 0 and not fragmented) or opcode not in (0, 1):
                raise Failure("invalid text fragmentation")
            message.extend(data)
            if final:
                return decode_json(message.decode("utf-8"))
            fragmented = True


class Rpc:
    def __init__(self, transport):
        self.transport = transport
        self.sequence = 0

    def call(self, method, params):
        self.sequence += 1
        request_id = self.sequence
        self.transport.send({"id": request_id, "method": method, "params": params})
        while True:
            response = self.transport.receive()
            if not isinstance(response, dict):
                raise Failure("non-object RPC message")
            if "method" in response:
                if not isinstance(response["method"], str):
                    raise Failure("invalid RPC notification")
                if "id" in response:
                    # Do not approve OR deny work belonging to the interactive owner.
                    raise Failure("server request requires the interactive owner")
                continue
            if type(response.get("id")) is not int or response["id"] != request_id:
                raise Failure("unexpected RPC response ID")
            if ("result" in response) == ("error" in response):
                raise Failure("invalid RPC response envelope")
            if "error" in response:
                error = response["error"]
                if not isinstance(error, dict) or type(error.get("code")) is not int or not isinstance(error.get("message"), str):
                    raise Failure("malformed RPC rejection")
                # Do not log server messages, which may contain report text.
                raise Rejected(method, error["code"])
            if not isinstance(response["result"], dict):
                raise Failure("invalid RPC result")
            return response["result"]


def check_owner(rpc, args):
    # These pinned releases explicitly exempt codex-backend from changing the
    # owning server's global originator/UA. Keep our actual purpose in title.
    hello = rpc.call("initialize", {"clientInfo": {
        "name": "codex-backend", "version": "orient-delivery-1", "title": "Orient delivery"}})
    if hello.get("codexHome") != args.codex_home:
        raise Failure("server CODEX_HOME mismatch")
    # Bind the leading product/version, not an incidental version token in
    # OS/client suffixes. Product name is not fixed: the pinned server's
    # retained global originator can differ (observed on Stars 0.163.0).
    user_agent = hello.get("userAgent")
    primary = re.match(r"\A[^/\r\n]+/([0-9]+\.[0-9]+\.[0-9]+)(?=[ \t]|\Z)", user_agent) if isinstance(user_agent, str) else None
    if primary is None:
        raise Failure("malformed server primary userAgent version")
    version = primary.group(1)
    if version != args.server_version:
        raise CompatibilityFailure(f"server version mismatch (expected {args.server_version}; observed {version})")
    rpc.transport.send({"method": "initialized", "params": {}})
    cursor = None
    seen = set()
    while True:
        page = rpc.call("thread/loaded/list", {"limit": 100, "cursor": cursor})
        data = page.get("data")
        if not isinstance(data, list) or any(not isinstance(item, str) for item in data):
            raise Failure("malformed loaded thread list")
        # Pinned 0.163 ThreadLoadedListResponse requires only data; an omitted
        # nextCursor is None/exhausted. Validate any supplied cursor even when
        # this page contains the owner, but do not exhaust later pages then.
        cursor = page.get("nextCursor")
        if cursor is not None and (not isinstance(cursor, str) or not cursor or cursor in seen):
            raise Failure("invalid loaded thread pagination")
        if args.thread in data:
            break
        if cursor is None:
            not_paused(args)
            raise KnownNotSubmitted("verified owner temporarily has no loaded target thread")
        seen.add(cursor)
    thread = rpc.call("thread/read", {"threadId": args.thread, "includeTurns": False}).get("thread")
    if not isinstance(thread, dict) or thread.get("id") != args.thread:
        raise Failure("thread identity mismatch")
    status = thread.get("status")
    if not isinstance(status, dict):
        raise Failure("malformed thread status")
    if status.get("type") in ("notLoaded", "systemError"):
        not_paused(args)
        raise KnownNotSubmitted("verified target is temporarily " + status["type"])
    if status.get("type") not in ("idle", "active"):
        raise Failure("unrecognized thread status")
    if thread.get("canAcceptDirectInput") is not True:
        raise Failure("thread cannot accept direct input")
    return status["type"]


def save_state(path, state):
    fd, temporary = tempfile.mkstemp(prefix=".dispatch-", dir=path.parent)
    try:
        with os.fdopen(fd, "w") as stream:
            json.dump(state, stream, sort_keys=True)
            stream.write("\n")
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
        directory = os.open(path.parent, os.O_RDONLY)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def not_paused(args):
    if any(os.path.lexists(marker) for marker in args.pause_marker):
        raise Paused("notifications are paused")


def deliver(args, report, deadline):
    not_paused(args)
    if args.probe:
        transport = WebSocket(args.socket, deadline)
        try:
            return check_owner(Rpc(transport), args)
        finally:
            transport.close()
    directory = Path(args.state_dir)
    directory.mkdir(mode=0o700, parents=False, exist_ok=True)
    info = directory.lstat()
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
        raise Failure("state directory must be private and owned by current user")
    key = hashlib.sha256(args.thread.encode()).hexdigest()
    with open(directory / (key + ".lock"), "a") as lock:
        os.chmod(lock.name, 0o600)
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise Failure("another delivery for this thread is in progress") from None
        path = directory / (key + ".json")
        if path.exists():
            previous = decode_json(path.read_text())
            if (not isinstance(previous, dict) or previous.get("thread") != args.thread
                    or previous.get("status") not in ("accepted", "rejected")):
                raise Failure("unresolved dispatch; inspect private state before any retry")
        transport = WebSocket(args.socket, deadline)
        try:
            rpc = Rpc(transport)
            check_owner(rpc, args)
            not_paused(args)
            state = {"thread": args.thread, "sha256": hashlib.sha256(report.encode()).hexdigest(),
                     "attempt": os.urandom(16).hex(), "time": time.time(), "status": "unknown"}
            # Commit BEFORE any potentially accepting bytes. A crash may conservatively
            # require reconciliation even if the request never left this process.
            save_state(path, state)
            try:
                result = rpc.call("turn/start", {"threadId": args.thread,
                    "input": [{"type": "text", "text": report}]})
            except Rejected as error:
                # Only protocol/parameter validation errors prove non-acceptance.
                # An internal server error may have occurred after submission.
                status = "rejected" if error.code in (-32600, -32601, -32602) else "unknown"
                state.update(status=status, diagnostic=str(error))
                save_state(path, state)
                if status == "unknown":
                    raise Failure(f"dispatch outcome unknown (RPC {error.code}); reconcile private state") from error
                raise
            except (Failure, OSError, ValueError, TypeError) as error:
                raise Failure(f"dispatch outcome unknown; reconcile private state: {error}") from error
            turn = result.get("turn")
            if (not isinstance(turn, dict) or not isinstance(turn.get("id"), str)
                    or not turn["id"] or turn.get("status") != "inProgress"
                    or turn.get("error") is not None):
                raise Failure("unconfirmed turn acceptance; dispatch remains unknown")
            state.update(status="accepted", turn=turn["id"])
            save_state(path, state)
            return "accepted"
        finally:
            transport.close()


def read_report(deadline):
    data = bytearray()
    while True:
        if not select.select([sys.stdin.buffer], [], [], remaining(deadline))[0]:
            raise Failure("stdin deadline exceeded")
        chunk = os.read(sys.stdin.fileno(), 65536)
        if not chunk:
            break
        data.extend(chunk)
        if len(data) > MAX_REPORT:
            raise Failure("report exceeds 2 MiB")
    if not data:
        raise Failure("report is empty")
    return data.decode("utf-8")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("thread")
    parser.add_argument("--socket", required=True)
    parser.add_argument("--codex-home", required=True)
    parser.add_argument("--server-version", required=True, choices=("0.160.1", "0.162.1", "0.163.0", "0.164.0", "0.165.0"))
    parser.add_argument("--state-dir", required=True)
    parser.add_argument("--pause-marker", action="append", required=True)
    parser.add_argument("--deadline", type=float, default=24.0)
    parser.add_argument("--probe", action="store_true", help="read-only owner check; no stdin or state write")
    args = parser.parse_args()
    if not 0 < args.deadline <= 25 or not args.thread or any(not os.path.isabs(p) for p in
            [args.socket, args.codex_home, args.state_dir, *args.pause_marker]):
        parser.error("deadline must be >0 and <=25 seconds; configured paths must be absolute")
    deadline = time.monotonic() + args.deadline
    try:
        report = "" if args.probe else read_report(deadline)
        outcome = deliver(args, report, deadline)
        print(f"Orient delivery: {outcome}", file=sys.stderr)
        return 0
    except (Failure, OSError, ValueError, TypeError) as error:
        if isinstance(error, KnownNotSubmitted):
            try:
                not_paused(args)
            except Paused as paused:
                error = paused
        print(f"Orient delivery failed [{type(error).__name__}]: {error}", file=sys.stderr)
        return exit_status(error)


if __name__ == "__main__":
    sys.exit(main())
