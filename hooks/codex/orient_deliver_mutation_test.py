#!/usr/bin/env python3
"""In-memory mutation controls: each intentionally unsafe variant must fail."""
import importlib.util
import io
from pathlib import Path
import types
import unittest

root = Path(__file__).parent
spec = importlib.util.spec_from_file_location("checks", root / "orient_deliver_test.py")
checks = importlib.util.module_from_spec(spec)
spec.loader.exec_module(checks)
source = (root / "orient_deliver.py").read_text()
mutations = {
    "ignore-operator-pause": ("if any(os.path.lexists(marker) for marker in args.pause_marker):", "if False:"),
    "accept-incidental-version": ("if version != args.server_version:", "if args.server_version not in user_agent:"),
    "erase-retryable-exit": ("return 75", "return 1"),
    "retry-all-generic-failures": ("return 1\n\n\nclass Rejected", "return 75\n\n\nclass Rejected"),
    "retry-operator-pause": ("return 78", "return 75"),
    "missing-owner-is-permanent": ('raise KnownNotSubmitted("verified owner temporarily', 'raise Failure("verified owner temporarily'),
    "unknown-status-is-retryable": ('raise Failure("unrecognized thread status")', 'raise KnownNotSubmitted("unrecognized thread status")'),
    "skip-loaded-proof": ("if args.thread in data:", "if True:"),
    "change-owner-global-identity": ('"name": "codex-backend"', '"name": "orient_delivery"'),
    "retry-unknown": ('not in ("accepted", "rejected")', 'not in ("accepted", "rejected", "unknown")'),
    "accept-failed-turn": ('turn.get("status") != "inProgress"', 'turn.get("status") not in ("inProgress", "failed")'),
    "internal-error-safe-to-retry": ('error.code in (-32600, -32601, -32602)', 'True'),
    "override-turn-settings": ('{"threadId": args.thread,\n                    "input":', '{"threadId": args.thread, "model": "wrong-model",\n                    "input":'),
}
for name, (before, after) in mutations.items():
    if source.count(before) != 1:
        raise SystemExit(f"mutation anchor changed: {name}")
    candidate = types.ModuleType("mutant")
    exec(compile(source.replace(before, after), str(root / "orient_deliver.py"), "exec"), candidate.__dict__)
    checks.delivery = candidate
    output = io.StringIO()
    suite = unittest.defaultTestLoader.loadTestsFromTestCase(checks.DeliveryTests)
    result = unittest.TextTestRunner(stream=output).run(suite)
    if result.wasSuccessful():
        raise SystemExit(f"UNDETECTED unsafe mutation: {name}")
    print(f"DETECTED {name}: {len(result.failures)} failures, {len(result.errors)} errors")
print("All essential-check mutation controls failed as intended.")
