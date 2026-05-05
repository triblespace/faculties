= When to Use Codex (and When Not To)

Codex is a separate agent runtime you can launch from your shell.
Treat yourself as the *control plane*, codex as the *data plane*:
you supervise, plan, integrate; codex grinds.

== When codex earns its weight

  - Long literature trawls (read 30 papers, summarise each).
  - Multi-round experiments (run a benchmark, tune a knob, rerun,
    compare).
  - Parallelisable code generation (port 12 similar files).
  - Anything where the *token cost of reading* dominates.

Codex has abundant token quota and no stateful supervision. You
have `/loop` and limited quota. Match the work to the tool.

== When codex is overhead

  - Short tasks (under 10 min wall-clock).
  - Tasks where supervising codex's output costs more time than
    doing it yourself.
  - Tasks where the answer is already in your context — codex
    would just re-derive it.

== Launch pattern

```sh
codex exec --dangerously-bypass-approvals-and-sandbox \
  --skip-git-repo-check \
  "$(cat /tmp/prompt.txt)" \
  < /dev/null > /tmp/log 2>&1
```

Run via Bash with `run_in_background: true` and *no trailing `&`*.
The `< /dev/null` is *required* — codex blocks on stdin
indefinitely otherwise. The `&` is *forbidden* — it detaches
codex from the shell and fires the task-completion notification
instantly.

== Prompt discipline

  - Inline all context (one paragraph, no "see file X").
  - State the specific deliverable path (`/tmp/codex_outputs/...`)
    and demand a heredoc.
  - Set a budget ("~15 min, stub if you run out").
  - Wrap the resulting `.typ` file as a wiki fragment yourself —
    don't have codex invoke `wiki create` directly. Sanity-check
    is easier from your side first.
