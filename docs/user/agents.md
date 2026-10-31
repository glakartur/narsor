# Running AI agents

How `AgentAction` states run an AI agent harness: sessions, read-only repositories and permissions. Harness invocation details are in [Building and testing](../dev/building-and-testing.md).

## `AgentAction` — running an AI agent

A state whose action is `type: agent` runs an AI coding agent in the
execution's workspace and takes its transition from the label the agent
returns.

```yaml
analyze:
  action:
    type: agent
    harness: claude-code          # optional; or: codex, github-copilot, opencode
    model: opus                   # optional, passed through verbatim
    workspace_access: read-only   # optional; default: read-write
    prompt: |
      Analyse the repository for: {{ inputs.description }}.
      Write the plan to plan.md.
    parameters:                   # optional, harness-specific but not access control
      max_turns: 20
    read_only_paths:              # optional, relative to the workspace
      - ../other-repo/src
  choices: [ready, ambiguous]
  transitions:
    ready: implement
    ambiguous: human_clarification
```

`harness` is **optional**. An action that names none uses the installation's
default harness: the operator's choice (`narsor-cli harness default set <harness>`,
or `h` in the TUI) when there is one, otherwise the configured
`agent_harness`, and failing that the built-in `claude-code` — so a workflow
without it is portable and still passes `validate`. The other side of that
bargain: its behaviour then depends on the environment, and the same file may
run a different agent on a laptop than in a container. Read the effective value
with `narsor-cli harness default` (the configured one with `narsor-cli config show`); a
workflow meant to run reproducibly should declare `harness:` explicitly.

```bash
narsor-cli harness default               # get: effective, configured, override, supported (--json: as JSON)
narsor-cli harness default set codex     # the operator's choice, until changed or reset
narsor-cli harness default reset         # back to the configured agent_harness
```

**A change of the default reaches work already under way**, not only new
tasks — at a clean boundary. An agent action reads the default once, at its
first attempt, and keeps that harness for its retries, reprompts and a
resumed session; the next agent action takes the new default, and so does the
conductor's next invocation. Only a `harness:` the workflow names itself pins
an action. A harness given when a remote task is created (`--harness`, or the
profile's) is the task's fallback, not a pin: the task follows the default
like any other. A remote task's runner hears the default with every answer of
the control plane; when the task's backend does not run that harness, or its
binary is not on the runner's `PATH`, the task stays on the harness it has and
reports it in its runner view (`default_harness_unavailable` in `narsor-cli task
show --json`; one line in the TUI's details) — information, not a failure of a
step.

`timeout` on an agent state works the same way: declared, it wins; absent, the
configured `agent_timeout` applies. Script and human states are untouched by
that setting — a script state that declares no limit still has none.

An agent that needs a person's answer without changing workflow stage returns
`{"status":"awaiting_input","question":"...","options":["..."]}` rather than an output label.
Narsor leaves the execution on that same agent state with status `input_needed`; the
normal `narsor-cli input <id> --field answer='...'` command resumes that state and
adds the answer to the next agent prompt. Harness, sandbox, permission and tool
failures remain action failures: the execution is `failed` on the current state
and no workflow transition is taken.

**The harness binary is a runtime prerequisite, like `git`.** It is never
bundled and never installed; a missing one is reported as an action error
naming the binary and saying it must be on `PATH`, not as
`No such file or directory`.

### Sessions

Every run is an agent session: `AgentSessionStarted` is written to the event
log before the harness starts, `AgentMessage` carries the agent's final text
(truncated — see below), and `AgentSessionCompleted` or `AgentSessionFailed`
closes it. The harness's own session id is captured from its output and
published two ways: as the context variable `agent_session_id`, and inside the
result under `data.agent_session`.

All harnesses are run in streaming mode by default (`--output-format
stream-json --verbose` for Claude Code, `--format json` for opencode, which
already streams NDJSON incrementally, and `exec --json` for Codex). Codex
accepts a result only after `thread.started`, a completed `agent_message`, and
a later `turn.completed`; `turn.failed` and top-level `error` remain failures.
Every line the harness process writes, to either
stream, is appended immediately to that attempt's own conversation record:
`{root}/executions/{id}/conversations/{action_execution_id}.jsonl` while the
attempt is open, compressed to `.jsonl.zst` once it closes. `AgentMessage`
still carries only the harness's final text — the event log is not a place
for megabytes of tool chatter — but nothing said over the stream is lost: the
conversation record is the byte-for-byte source of truth for what the harness
actually said, in order, prompt included. `narsor-cli conversation <id> <attempt>`
(and the TUI's *Rozmowa* tab) read it; nothing else in narsor reads it. A conversation
record's key is the attempt's `action_execution_id` — the same idempotency key
D24 already assigns fresh on every attempt — so a retried step never shares
one record between its attempts, and the record survives a local takeover
untouched, in the same directory the checkpoint bundle lives beside.

Resumption is **half-wired on purpose.** `parameters.resume_session: <id>`
continues a known session (`--resume` / `--session`), but `parameters` is not
template-expanded — only `prompt` is — so a session captured by one state
cannot yet be chained into the next by writing
`{{ context.variables.agent_session_id }}`. The id is captured and exposed so
that chaining becomes a one-line change in the template scope, and nothing more.

### `read_only_paths`

Extra directories the agent may look at, resolved against the workspace. They
become `--add-dir` for `claude-code`; `opencode run` and Codex have no safe
read-only equivalent, so they are named in the message instead. Either way this is about **visibility,
not enforcement**: read-only access to secondary repositories is enforced by the
workspace provisioner's permission bits (D15), and would still be enforced if
the flag were left out.

### Permissions, non-interactivity, and what that costs

The narsor runs unattended, so a harness that stopped on a permission
prompt would hang a workflow until its timeout. Both harnesses are therefore run
in their batch mode, where nothing can be prompted for.

The portable default is **`read-write`**. The adapter maps it to Claude's
`acceptEdits`, OpenCode's `--auto`, and Codex's `workspace-write`. An action
that must inspect only sets `workspace_access: read-only`; it maps to Claude's
`plan` mode, omits OpenCode's auto-approval flag, and uses Codex's `read-only`
sandbox. Codex always passes `approval_policy=never` and never allows
`danger-full-access`; its operator-provided environment and profile are
otherwise preserved for authentication and skills.

**The security consequence is real and is not hypothetical.** Containerisation
(D14) is still deferred, so the only boundary around an agent is D15's
filesystem layout and permission bits — which protects against a mistake, not
against a determined or prompt-injected agent. In particular, OpenCode's
read-write mapping auto-approves its tool requests; without container isolation
this can reach beyond the workspace. Use writable actions only for workflows
and prompts you trust.
