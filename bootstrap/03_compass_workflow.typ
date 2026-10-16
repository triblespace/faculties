= Compass Goals Workflow

`compass` is the goal/task tracker. Use it instead of an ephemeral harness
task list when work should survive a session or move between agents.

== Statuses

Compass presents four default statuses first:

  - `todo` — queued, not started
  - `doing` — actively in progress
  - `blocked` — waiting on something external
  - `done` — finished, with an outcome note

These are defaults, not a closed protocol. Projects may use other status names;
Compass records them without assigning special transition or gate semantics.

== Daily flow

  + `compass list` — see active goals
  + Pick one and run `compass move <id> doing`
  + Add notes as decisions and context arrive: `compass note <id> "..."`
  + Record the outcome in a final note
  + Run `compass move <id> done`
  + Repeat

Set `$PERSONA` (or pass `--persona`) when attribution matters. Status events
and notes then carry the acting relations person. Attribution says who made an
observation; it does not grant authority or change workflow semantics.

Every note has a stable ID, printed when it is created and by `compass show`.
Write `@label` in a note (or a goal title) to mention a persona or a
Relations group; use repeatable `--tag` values for topics, `--ref` for opaque exact
artifact references, and `--supersedes <full-note-id>` when one note records a
newer observation. Recognized `[text](faculty:hex)` links are stored as
references automatically. Supersedes is provenance only: Compass still shows
both notes and never infers a current one.

== Workflow neutrality

Compass is a durable queue and journal, not a release gate. Review, acceptance,
publication, and deployment policies belong at their own boundaries. When a
peer reviews an artifact, keep the report diagnostic and descriptive: name the
exact artifact, observation, location, and evidence. Do not make that report
own the goal status or stop the author from continuing. If the observation
still applies to the current artifact, capture the repair as an ordinary goal;
if later work removed the cause, close the finding without ceremonial work.

This separation matters because development can resolve a diagnosis by
deleting or reshaping the surrounding design. A prescriptive gate tends to
freeze both the candidate and the reviewer's proposed solution too early.

== Hierarchy

`compass add "<title>" --parent <id>` creates a sub-goal. Use this when a goal
naturally breaks into several independently useful steps. Do not sub-goal
trivial next actions; the conversation is the right level for those.

== Tags

Compass tags work like wiki tags. Common ones are `#meta`, `#bootstrap`, and
`#parking`, plus project-specific tags such as `#myproject` or `#deploy`.
Tag a goal with a persona's own relations label to assign it to that persona:
Orient then reports other people's status changes and notes on it as directed
news. Every other tag, a group name such as `crew` included, is only a topic.
To address someone once, mention them: an `@label` in a title or note names a
persona label, alias or group name (case-insensitively), and Orient tells that
persona, or every member of that group, about it. A mention starts the text or
follows a non-alphanumeric character, so an email address mentions nobody, and
an `@label` that matches nobody is warned about while the write still succeeds.
On the command line a leading `@` reads the text from a file, so text that
begins with a mention starts with `@@` instead (`@@ada please look`).
Writing a note or moving a goal does not subscribe you to it.

== Scope control: parking lot

Mid-task ideas that would take more than two minutes go in Compass with a
`#parking` or `#later` tag. Then return to the current goal. Near the end of a
session, promote at most a couple of parked items and leave the rest queued.

== When not to use Compass

  - In-conversation planning for the next few minutes
  - Scratch state that has no value after the current turn

Next stop: [Wiki Fragment Style Guide](wiki:82129c70b693f7e2d781d78ac5efbb86).
