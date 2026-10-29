# Persistent Orient delivery to Codex

One `orient daemon` keeps the pile open, observes changes, and delivers each
complete report on stdin to `orient_deliver.py`. This direct adapter connects
to the existing owner's local App Server; it does not start a server, resume a
thread, create a conversation owner, change model settings, or spool reports.

Before enabling the daemon, remove the old Orient SessionStart,
UserPromptSubmit and Stop entries from the host's hook configuration and
stop its owned one-shot wrapper. Remove watcher-first/rearm instructions from
the host's agent instructions too. Do not run prompt-time `poll --peek` or
log forwarding alongside the daemon: they are competing delivery paths.
Honor the operator pause; never automatically replay held legacy queue entries.
Changing this source does not change an existing host's launcher or readiness gate.

## Bind the existing owner

Explicitly select the pile, persona, exact thread, absolute callback path, local
socket, expected `CODEX_HOME`, supported server version, private dispatch-state
directory, and pause-marker paths. The state directory's parent must exist;
the directory itself must be owned by the current user with no group/other access.
This source accepts server versions **0.160.1 and 0.163.0** only; a different
host's deployed adapter whitelist is not this source's compatibility contract.

Example wiring, after those host-specific values and the sole owner are reviewed
(all path variables below must be absolute; this is not an installation command):

```sh
"$ORIENT_BIN" \
  --pile "$PILE" --persona "$PERSONA" daemon \
  --retry-known-not-submitted \
  --callback /usr/bin/python3 --callback-arg=-B \
  --callback-arg="$ORIENT_DELIVER" --callback-arg="$CODEX_THREAD_ID" \
  --callback-arg=--socket --callback-arg="$CODEX_OWNER_SOCKET" \
  --callback-arg=--codex-home --callback-arg="$CODEX_HOME" \
  --callback-arg=--server-version --callback-arg="$CODEX_SERVER_VERSION" \
  --callback-arg=--state-dir --callback-arg="$ORIENT_DISPATCH_STATE" \
  --callback-arg=--pause-marker --callback-arg="$ORIENT_PAUSE_MARKER"
```

The adapter checks socket ownership, performs `initialize` / `initialized`,
verifies the returned home and leading user-agent version, and checks the exact
loaded thread's identity, status and direct-input capability. It validates
`idle` or `active`, then sends **one `turn/start`** with only `threadId` and
the report input, relying on the pinned owner's atomic start-or-steer contract.
It does not choose a client-side `turn/steer` branch or retry a racing submission.
The [official lifecycle overview](https://learn.chatgpt.com/docs/app-server#lifecycle-overview)
describes `turn/start` and `turn/steer` generally; it is background, not proof of
this pinned integration's behavior on another server version.

## Known-not-submitted retry, not delivery replay

`--retry-known-not-submitted` is opt-in and trusts this callback's exit-75
promise; do not enable it for an arbitrary callback that uses 75 differently.

- **75:** after consuming complete stdin, the adapter has sent no potentially
  accepting request and has verified either an absent target in an exhausted,
  well-formed loaded list, or the exact target's `notLoaded` / `systemError`
  status. Rust recognizes this outcome only after complete stdin was written
  and before the callback deadline.
- **78:** operator pause or expected-version mismatch; terminal, no retry.
- **Other failures:** malformed/unknown identity or protocol state, socket/RPC
  errors, partial stdin/EPIPE, signal, timeout, rejection or uncertain acceptance
  remain terminal. A generic failure is never promoted to known-not-submitted.

The daemon awaits asynchronously with delays **5s, 20s, then 60s repeatedly**:
the delay is capped, not the attempt count. It holds the same immutable report;
it does not poll/select new news while that report is pending. Acceptance resets
the backoff. There is no retry queue or second notification owner.

Every adapter attempt rechecks pause, identity and version. Pause is checked
again before submission and before returning 75; a pause during backoff is
observed on the next attempt, not by a separate watcher. `--run-for` shares a
monotonic deadline with callback bounds and backoff. Async backoff yields to
SIGINT/SIGTERM; an active synchronous callback still has its bounded timeout,
and unrelated synchronous upkeep is not made instantly interruptible.
Default callback timeout is 30s (`--callback-timeout`); the adapter's own deadline
defaults to 24s and must be >0 and <=25s. Do not replace this contract with an
unconditional service restart or a separate sleep/rearm loop.

Before any potentially accepting request, the private dispatch ledger is
persisted as `unknown`; only confirmed acceptance changes it to `accepted`.
An unresolved/wrong-thread record blocks delivery **before** any readiness-75
path. Unknown outcomes require operator reconciliation, never automatic replay.
The ledger records dispatch metadata, not a spool of report bodies.

## Preparation, acceptance and presentation

The daemon freezes a `report prepared at ... UTC; delivery may be delayed`
header before the first attempt. It dates report preparation, not every
underlying observation, and stays unchanged across retries. UTF-8 text and
trailing newlines travel on stdin, not a shell command or message argv; this
adapter limits reports to 2 MiB. Callback stdout is discarded; diagnostics use
stderr rather than duplicating report bodies into tool output.

Successful callback exit means transport acceptance, not that the agent read or
handled the work. Existing `Presented` receipts are committed only afterward;
ordinary news receipts receive a post-acceptance timestamp. A later receipt
failure is fatal and does not retract acceptance or trigger automatic replay.
A crash between acceptance and receipt storage can still duplicate news on a
later run: this is best effort, not exactly-once delivery. Cancellation of a
known-unsent pending report writes no `Presented`. Habit completion is separate.

## Legacy files

`orient_queue.sh` and its `orient_queue_test.sh` mock test still ship for the
legacy `codex queue --thread ... --message ...` route. They do **not** implement
the direct adapter's dispatch ledger or trusted-75 contract and are not the
primary wiring above. Retain held old entries for explicit reconciliation;
do not start both routes. Adapter source and offline fixtures are
`orient_deliver.py`, `orient_deliver_test.py` and `orient_deliver_mutation_test.py`.
Ordinary manual `wake`, `show`, `poll` and one-shot `wait` remain available,
but must not be added as competing automatic notification paths.
