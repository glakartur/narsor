# The conductor

The conductor is the per-execution assistant that talks to the running workflow and proposes actions you can confirm or reject.

## The conductor

The daemon runs a **conductor** beside every execution: an assistant on the
installation's default harness (the operator's choice — `h` in the interface,
`narsor-cli harness default set` — or else the one `agent_harness` names) that
reads the task and never writes it. There is no per-task choice: a changed
default reaches the conductor of every task, running ones included, from its
next invocation; the *Dyrygent* tab names the harness it runs on. It has one history per
execution, with its chat, kept under the daemon's data directory, so it
survives a restart of the daemon and of the interface.

It steps in on its own in three places, each with fixed limits:

* **A step without a usable output label.** The engine first asks the agent
  once more in the same session. If that does not help either, the conductor
  reads the answers and either chooses the output (with the `data` and
  `variables` it needs), asks the agent once more itself, or hands the step to
  the operator as a **problem** with its analysis. At most two extra questions
  reach the agent in total. A choice it made can be undone.
* **A step that failed** after its `retry_policy` ran out. The execution stays
  `failed`. The conductor diagnoses the failure and proposes what to do; it may
  restart the step once by itself, with a note, and only when no attempt of the
  step can have had a side effect. It never chooses a transition after a
  failure — that can only be proposed.
* **A `human` gate.** It may add a summary and a recommendation to the form.
  The gate always waits for the operator.

Every decision it takes is an event with its confidence and rationale
(`narsor events`). If the conductor itself cannot answer, its invocation is
repeated once — also across a daemon restart — and then the operator gets a
problem with the raw error; a failure of the conductor never triggers the
conductor again.

**The chat** (`c` in the task view, or `narsor-cli conductor chat|history`) lets
you ask about the task. Whatever it proposes — answering the pending input,
telling the agent something, restarting the failed step, taking a transition,
cancelling — happens only once you confirm it (`narsor-cli conductor confirm`,
`reject`, `undo`). A proposal about a moment the execution has moved on from
is refused (`409`). Only the conductor's latest choice of an output can be
undone, and never on a cancelled execution.

**Read-only, per harness.** On `claude-code` and `codex` the conductor runs in
the task's workspace in the harness's own read-only mode (`plan`,
`--sandbox read-only`). `opencode` has no such mode, so there the conductor
runs in an empty scratch directory and knows the task only from the snapshot
it is given: on opencode, read-only means **isolation from the workspace
directory, not a sandbox**.
