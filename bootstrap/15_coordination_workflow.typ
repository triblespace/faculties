= Recipe: Multi-Agent Coordination

How two agents working on the same pile (or on team-mesh-synced
piles) hand work back and forth without stepping on each other.
Chains `relations`, `local_messages`, `orient`, and `compass`.

== Why a recipe

Each faculty in isolation is obvious. The non-obvious thing is
the *handshake order* — which faculty signals which event so the
other agent picks it up. Skip steps and you get racy work
(both agents claiming the same goal) or silent drops (a message
that was sent but never read).

== The recipe — Agent A hands work to Agent B

```sh
# Setup (once, on each pile):
relations.rs add jp --display-name "JP" --affinity "user"
relations.rs add agent-b --display-name "Agent B" --affinity "teammate"

# Agent A: claim the goal first.
GOAL=<existing-goal-id>
compass.rs move $GOAL doing
compass.rs note $GOAL "Claimed by agent-a, draft pending"

# Agent A: do partial work, then prepare hand-off.
compass.rs note $GOAL "Draft at wiki:<frag-id>; need agent-b to verify the calculation"
compass.rs move $GOAL blocked   # signals "external dependency"
local_messages.rs send agent-b "Goal $GOAL ready for review — see wiki:<frag-id>"

# Agent B (later, possibly different session):
orient.rs show                  # surfaces the new message + blocked goal
local_messages.rs ack <msg-id>  # read receipt
compass.rs move $GOAL doing
compass.rs note $GOAL "Picked up by agent-b for review"

# Agent B does the work, hands back:
compass.rs note $GOAL "Verified §3.2; numbers correct. Pushed back to A."
local_messages.rs send agent-a "Done with review on $GOAL"
compass.rs move $GOAL doing     # NOT done — A still owns the goal
```

== Why each step

  - *relations.rs once per pile*: handles resolve through this
    registry. Without it, `local_messages.rs send agent-b ...`
    can't address the recipient.
  - *Status change before sending the message*: `compass.rs move $GOAL blocked` first.
    the status change is the durable signal; the message is
    the polite notification. If the message is missed, the next
    `orient.rs show` from B still surfaces the blocked goal.
  - *Notes on every transition*: the goal's history is the
    audit trail. "Claimed by", "Draft at", "Verified", "Pushed
    back" — each note records what happened and who did it.
  - *Don't move to done across the handshake*: only the agent
    who *originated* the goal should mark it done. B verifies
    and hands back; A confirms and closes.

== orient.rs wait for idle agents

If Agent B is running a `/loop` or sitting idle, replace
`orient.rs show` with `orient.rs wait` — that blocks until any
relevant branch changes, including gossip-merged remote writes
through `pile net sync`.

```sh
# Agent B's idle loop:
while true; do
  orient.rs wait    # blocks until something arrives
  # ...handle whatever showed up...
done
```

== Conflict resolution

Pile branches are append-only with `cat` union for compass and
local-messages, so concurrent writes never overwrite. Two agents
both moving the same goal to `doing` produces two status events
in chronological order; `compass.rs list` shows the latest.

For genuine ambiguity ("we both started working on this"), the
fix is a quick `local_messages.rs send <other> "I'm taking this
one — you grab X"` — a coordination layer the system supports
but doesn't enforce.

== Cross-references

  - "Local Messages: Agent-to-Agent Direct Messaging"
  - "Relations: People and Handle Mappings"
  - "Orient: The Situation-Snapshot Faculty"
  - "Compass Goals Workflow"
  - "Teams: Capability-Based Membership" — for inter-pile sync
    over a real network (gossip + DHT), the auth-arc tools
    underneath this recipe.
