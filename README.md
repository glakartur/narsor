# Orchestrator

A workflow engine for tasks carried out by AI agents and by people. A process is
described in YAML (a graph of states, actions and transitions) and the
orchestrator runs it step by step, persisting every state and every event to
disk — so that an interrupted execution can be recovered and the whole run can
be audited afterwards.

Design documentation (written in Polish, and normative):

- [`ARCHITECTURE_DECISIONS.md`](ARCHITECTURE_DECISIONS.md) — decisions D1–D46
  with their rationale.
- [`DOMAIN_MODEL.md`](DOMAIN_MODEL.md) — the domain model, the directory layout,
  the order in which events are emitted.
- [`WORKFLOW_SCHEMA.md`](WORKFLOW_SCHEMA.md) — the contract of the YAML file:
  sections, action types, decisions, validation rules.
- [`docs/plans/`](docs/plans/) — the implementation plans.

---

## Prerequisites

- **Rust** (2021 edition) with `cargo`.
- **`git` on `PATH` — a runtime prerequisite.** The orchestrator clones
  repositories by shelling out to the `git` command as a child process.
  `libgit2` was deliberately rejected, because it honours neither
  `~/.ssh/config` nor credential helpers nor `url.*.insteadOf` rules — which is
  exactly the configuration access to private repositories rests on. A missing
  `git` only shows up when a `git` workspace is provisioned, not at startup.
- **An agent harness on `PATH` — `claude` or `opencode` — a runtime
  prerequisite too**, and only for workflows that have an `agent` state. See
  [`AgentAction`](#agentaction--running-an-ai-agent). A missing one shows up
  when such a state is entered, not at startup.
- A POSIX system. Read-only access to secondary repositories is enforced with
  file permission bits.

### Building and testing

```bash
cargo build              # builds target/debug/orchestrator
cargo test               # the whole suite (unit + integration)
cargo build --release    # release build
```

The integration tests run against a real filesystem and spawn real child
processes — always in temporary directories, never inside the repository.

---

## Quick start

```bash
# 1. Check the definition
orchestrator validate tests/fixtures/cli/greet.yaml

# 2. Run the workflow
orchestrator run tests/fixtures/cli/greet.yaml \
    --input "greeting=Good morning" \
    --input "script_path=$PWD/tests/fixtures/cli/announce.sh"

# 3. See what is going on
orchestrator list
orchestrator show <EXECUTION_ID>

# 4. Answer the question the workflow asked
orchestrator input <EXECUTION_ID> --field approval_status=approved

# …or drive all of that from the full-screen interface
orchestrator tui
```

---

## Global options

| Option | Default | Meaning |
|---|---|---|
| `--root <DIRECTORY>` | `./.orchestrator` | The data directory: executions and workspaces. Applies to every subcommand, `tui` included. |
| `--help`, `--version` | — | Help and version. |

`tracing` logs go to **stderr** and the output of the command to **stdout**, so
that `orchestrator list | grep paused` keeps working with `RUST_LOG=debug` set.
The exception is `tui`, which owns the whole terminal: see
[`tui`](#tui) below.

---

## Subcommands

### `validate <FILE>`

Static validation of a definition against `WORKFLOW_SCHEMA.md` §5: graph
consistency, well-formed actions and decisions, references to variables. It
reports **every** violation at once, not just the first one it meets.

```console
$ orchestrator validate tests/fixtures/implement_feature.yaml
The definition is valid: implement-feature (version 1), states: 9, initial state: analyze.

$ orchestrator validate tests/fixtures/invalid/five_violations.yaml
Definition 'tests/fixtures/invalid/five_violations.yaml' is invalid — 5 violations found:
  - ...
```

### `run <FILE> [--input NAME=VALUE]…`

Creates an execution and drives it until it reaches a terminal state, pauses, or
fails. The execution id is printed on the first line.

`--input` values are parsed **according to the type declared in the
definition**: `string` and `repository` as text, `integer` as a whole number,
`boolean` as `true`/`false`. That is why `--input is_experimental=true` reaches
the context as a boolean and not as a string. Missing required inputs are
reported **before** anything is written to disk.

```console
$ orchestrator run tests/fixtures/cli/greet.yaml \
      --input "greeting=Good morning" \
      --input "script_path=$PWD/tests/fixtures/cli/announce.sh" \
      --input is_experimental=true
Created execution: 4f1c8f0e-...-9b21
Status: paused
Current state: approve
Human input expected: Is the announcement all right?
  --field approval_status=…  (Select, required, allowed values: approved, rejected)
```

**Where the workspace comes from.** In this order:

1. `globals.workspace: { type: git, repository: …, ref: … }` — the `repository`
   field may hold a `{{ inputs.* }}` template, which is expanded before use; the
   result is handed to `git clone` (a URL or a local path).
2. `globals.workspace: { type: local, path: … }` — the contents of that
   directory are copied into the workspace.
3. No `workspace` section — an empty directory
   `{root}/tasks/{execution_id}/code/workspace` is created. That is enough for
   workflows whose scripts live outside the repository.

Everything checkable without touching the disk (a well-formed remote, the
existence of a local path) is checked **before** the execution is created. The
clone itself happens afterwards, because the task directory is named for the
execution id. If the clone fails the execution is marked `failed` rather than
left `running`.

The path of the definition file is recorded in `state.json`
(`metadata.cli.definition_path`), so `input` and `resume` do not have to be
given it again.

### `list`

Lists the executions in the data directory, newest first. An empty store is not
an error.

```console
$ orchestrator list
ID                                     STATUS     STATE                  WORKFLOW
4f1c8f0e-...-9b21                      paused     approve                cli-greet
```

### `show <EXECUTION_ID>`

The status, the current state, the workspace and the context variables; plus the
question and its form fields, when the execution is waiting on a human.

```console
$ orchestrator show 4f1c8f0e-...-9b21
Execution:     4f1c8f0e-...-9b21
Workflow:      cli-greet (version 1)
Status:        paused
Current state: approve
Started:       2026-09-23T19:43:49.018665031+00:00
Workspace:     /tmp/data/tasks/4f1c8f0e-...-9b21/code/workspace
Context variables:
  announced = "Good morning"
  greeting = "Good morning"
  is_experimental = true
```

### `events <EXECUTION_ID> [--follow]`

The event history in sequence order: number, timestamp, type, description.
`--follow` keeps appending new events until the execution reaches a terminal
state.

```console
$ orchestrator events 4f1c8f0e-...-9b21
    1  2026-09-23T19:43:49+00:00  workflow_started       Workflow 'cli-greet' started
    2  2026-09-23T19:43:49+00:00  state_entered          Entered state 'announce'
    3  2026-09-23T19:43:49+00:00  action_started         Action 'script' started
```

### `input <EXECUTION_ID> [--field NAME=VALUE]… [--label LABEL]`

Delivers a human answer to a paused execution and resumes it. `--field` fills in
the form fields declared by the `human` action; `--label` names the output label
explicitly — needed when the action has no fields, or when it has several and
the label cannot be derived from a single `select` field. `--file` points at the
definition when the file has moved.

```console
$ orchestrator input 4f1c8f0e-...-9b21 --field approval_status=approved
Input delivered to execution 4f1c8f0e-...-9b21.
Status: completed
Current state: completed
```

### `resume <EXECUTION_ID>`

Resumes an execution that was paused by something **other** than a question for
a human (a manual `pause`, for instance). An execution waiting for an answer
needs `input`, not `resume` — that is the guard against delivering the same
answer twice.

### `tui`

The full-screen terminal interface: a `lazygit`-shaped console over the same
engine the CLI drives. It honours the global `--root`, so
`orchestrator --root /srv/data tui` shows that data directory.

```text
┌ Executions ────────┬ State and context ─────────┐
│ the execution list │ id, workflow, status,      │
│ (id, status,       │ current state, permitted   │
│  state, workflow)  │ outputs, override, context │
│                    │ variables                  │
│                    ├ Events ────────────────────┤
│                    │ the live event stream, with│
│                    │ the channel's health and a │
│                    │ follow indicator in the    │
│                    │ panel title                │
├────────────────────┴────────────────────────────┤
│ context-sensitive shortcut bar                  │
└─────────────────────────────────────────────────┘
```

The split is proportional, so a resize preserves the arrangement; only the
shortcut bar has a fixed height. Modals (the input form, the transition menu,
the override confirmation, the key map) are centred overlays drawn on top.

Updates come from `Engine::subscribe`, not from polling the disk; the event log
is read once, to backfill the history of a newly selected execution. A
broadcast channel that lags or closes is shown as an explicit indicator in the
Events panel title rather than a view that silently stops moving.

**Key map** (also available in the interface itself, under `?`):

| Context | Key | Action |
|---|---|---|
| Panels | `Tab` / `Shift+Tab` | next / previous panel |
| Panels | `j` / `↓`, `k` / `↑` | move the focused panel's cursor |
| Panels | `g` / `Home`, `G` / `End` | first / last entry |
| Panels | `r` | reload the list and the selection from the store |
| Panels | `s` | run or resume the selected execution |
| Panels | `i` | open the human-input form |
| Panels | `t` | open the manual transition menu |
| Events panel | `f` | toggle following the stream |
| Panels | `?` | open the key map |
| Panels | `q`, `Esc` | quit |
| Input form | `Tab` / `↓`, `Shift+Tab` / `↑` | next / previous field |
| Input form | `←` / `→` | cycle a `select` value or the output label |
| Input form | `space` | same, on a `select` row or the label row |
| Input form | printable keys, `Backspace` | type into a `text` field |
| Input form | `Enter`, `Esc` | submit, cancel |
| Transition menu | `j` / `↓`, `k` / `↑` | pick an entry |
| Transition menu | `o` | toggle override mode (only when the state declares `allow_manual_override`) |
| Transition menu | `Enter` | apply the choice (in override mode: ask for confirmation first) |
| Transition menu | `Esc` | cancel |
| Override confirmation | `y`, `n` / `Esc` | confirm, cancel |
| Key map | `Esc`, `?`, `Enter` | close |

**What it can do.** Browse every execution in the data directory and its
context; watch the live event stream; run a `running` execution on or resume a
`paused` one (`s`); fill in and submit a human-input form (`i`), which goes
through `Engine::deliver_human_input` exactly as `orchestrator input` does;
choose a declared output of the current state, or — with `o`, and only where the
state declares `allow_manual_override` — **force a transition to an arbitrary
state** after a separate `y`/`n` confirmation. A forced transition is written to
the log so it can be audited: a `DecisionMade` naming the `human_override`
provider and carrying the reason, then `HumanTransitionSelected`, `StateExited`
and `TransitionTaken`. The execution is parked `Paused` at the target state (or
finished, when the target is terminal) and the operator resumes it explicitly.

**What it cannot do.** It cannot *create* an execution: there is no screen for
picking a definition file and its inputs, so use `orchestrator run` for that and
then select the new execution in the list (`r` reloads it). Conversely the
override — a forced transition to an arbitrary state — currently exists **only**
in the TUI; there is no CLI equivalent for it.

**Logging.** `tui` takes over the alternate screen, so a log line on stderr
would be written over the panels. With `RUST_LOG` set, the interface's logs are
appended to `{root}/orchestrator.log` instead; with `RUST_LOG` unset no
subscriber is installed at all and the records are dropped. Either way nothing
reaches the terminal while the interface is up. Every other subcommand keeps
logging to stderr.

---

## `AgentAction` — running an AI agent

A state whose action is `type: agent` runs an AI coding agent in the
execution's workspace and takes its transition from the label the agent
returns.

```yaml
analyze:
  action:
    type: agent
    harness: claude-code          # or: opencode
    model: opus                   # optional, passed through verbatim
    prompt: |
      Analyse the repository for: {{ inputs.feature_description }}.
      Write the plan to plan.md.
    parameters:                   # optional, harness-specific
      permission_mode: acceptEdits
    read_only_paths:              # optional, relative to the workspace
      - ../other-repo/src
  outputs: [ready, ambiguous]
  transitions:
    ready: implement
    ambiguous: human_clarification
```

**The harness binary is a runtime prerequisite, like `git`.** It is never
bundled and never installed; a missing one is reported as an action error
naming the binary and saying it must be on `PATH`, not as
`No such file or directory`.

### How each harness is invoked

Both are run non-interactively, in the workspace, with
`ORCHESTRATOR_ACTION_ID` in the environment (D24), stdin closed, and both pipes
drained while the agent works.

```text
claude   --print --output-format json \
         [--add-dir <read_only_paths…>] [--model <model>] [--max-turns <n>] \
         [--append-system-prompt <text>] [--resume <session-id>] \
         [--allow-dangerously-skip-permissions] --permission-mode <mode> \
         <prompt>

opencode run [--model <provider/model#variant>] [--agent <agent>] \
         [--session <session-id>] [--fork] [--auto] --format json \
         <prompt>
```

`--print` / `run` are what make the run a batch run; `json` output is what
carries the session id as well as the text. The flag that takes exactly one
value is always the last one before the prompt, so a prompt can never be read
as a flag's argument.

`parameters` is the per-action, harness-specific part: `permission_mode`,
`max_turns`, `append_system_prompt`, `dangerously_skip_permissions`,
`resume_session` for `claude-code`; `agent`, `auto_approve`, `fork`,
`resume_session` for `opencode`. An unknown value or a wrong type is an error
before anything is run — never a silent default.

### The JSON output contract

The agent is asked — the contract is appended to every prompt — to finish with
a single JSON object:

```json
{ "output": "ready", "data": {}, "variables": { "plan": "plan.md" }, "artifacts": ["plan.md"] }
```

`output` is the only required key, and it must be one of the state's declared
`outputs`; `variables` reach the context, `artifacts` are registered, exactly
as for a script action.

Agents do not reliably emit clean JSON, so the answer is read robustly: a bare
object, a fenced `json` code block, or an object trailing any amount of prose all
work, and the **last** object carrying an `output` key wins. But **when no
usable label can be found, the action fails** — it is never guessed, and never
defaults to `success`. In an unattended pipeline a guessed label is a silently
wrong transition, which is worse than a failure an operator can see. The state's
`retry_policy` gives a hallucinating agent another attempt (D20); escalating to
a person afterwards is the state's own business.

### Sessions

Every run is an agent session: `AgentSessionStarted` is written to the event
log before the harness starts, `AgentMessage` carries the agent's final text,
and `AgentSessionCompleted` or `AgentSessionFailed` closes it. The harness's own
session id is captured from its output and published two ways: as the context
variable `agent_session_id`, and inside the result under
`data.agent_session`.

Resumption is **half-wired on purpose.** `parameters.resume_session: <id>`
continues a known session (`--resume` / `--session`), but `parameters` is not
template-expanded — only `prompt` is — so a session captured by one state
cannot yet be chained into the next by writing
`{{ context.variables.agent_session_id }}`. The id is captured and exposed so
that chaining becomes a one-line change in the template scope, and nothing more.

### `read_only_paths`

Extra directories the agent may look at, resolved against the workspace. They
become `--add-dir` for `claude-code`; `opencode run` has no equivalent flag, so
they are named in the message instead. Either way this is about **visibility,
not enforcement**: read-only access to secondary repositories is enforced by the
workspace provisioner's permission bits (D15), and would still be enforced if
the flag were left out.

### Permissions, non-interactivity, and what that costs

The orchestrator runs unattended, so a harness that stopped on a permission
prompt would hang a workflow until its timeout. Both harnesses are therefore run
in their batch mode, where nothing can be prompted for.

The default is deliberately **not** the most permissive setting:

- `claude-code` runs with `--permission-mode acceptEdits`. The agent edits files
  in its workspace on its own; anything heavier is refused rather than waited
  on. `parameters.permission_mode` accepts `acceptEdits`, `auto`,
  `bypassPermissions`, `manual`, `dontAsk` and `plan`, and `bypassPermissions`
  additionally passes the CLI's own `--allow-dangerously-skip-permissions`.
- `opencode` runs **without** `--auto`. Set `parameters.auto_approve: true` to
  auto-approve every permission that is not explicitly denied. Without it, a
  tool call opencode would have asked about is simply not granted, so an action
  that needs one either finishes without doing the work or sits until the state's
  `timeout` — which is the reason to set a `timeout` on agent states.

**The security consequence is real and is not hypothetical.** Containerisation
(D14) is still deferred, so the only boundary around an agent is D15's
filesystem layout and permission bits — which protects against a mistake, not
against a determined or prompt-injected agent. An agent run with
`bypassPermissions` or `auto_approve` has, for practical purposes, the
orchestrator's own access to the machine: your files, your credentials, your
network. Turn either on for a workflow you wrote, over a repository you trust,
and never for an untrusted workflow or an untrusted prompt.

---

## Exit codes

The exit codes are a contract for CI:

| Code | Meaning |
|---|---|
| `0` | The command did what it was asked. |
| `1` | A definition failed validation, an execution failed, an id is unknown, or the store could not be read. |
| `2` | Incorrect usage: bad argument syntax, an unknown input, a missing required input. |

The distinction is deliberate: **a failed workflow is code `1`** — the engine
did exactly what it was asked, and the process ended in a terminal state that
means failure. **A missing `--input` is code `2`**, because nothing was run.
Code `2` is also what the argument parser itself returns, so both sources of
incorrect usage are indistinguishable to a script.

---

## The YAML contract in brief

The full contract is `WORKFLOW_SCHEMA.md`. Two points about `terminal_states`,
because it accepts two forms:

```yaml
# Mapping form — every terminal state names the status it produces.
# Allowed values: completed, failed, cancelled.
terminal_states:
  completed: completed
  failed: failed
  abandoned: cancelled

# List form — the older one. Every entry means `completed`.
terminal_states:
  - completed
  - failed
```

The difference matters: under the list form a state called `failed` still
finishes the execution as `completed`, because the list declares only *which*
states are terminal and not what they mean. Reach for the mapping form when a
state is supposed to end the run as a failure or a cancellation.

---

## The on-disk layout

Under the `--root` directory (`./.orchestrator` by default):

```text
{root}/
├── executions/{execution_id}/
│   ├── state.json                  # WorkflowExecution: status, current state, context
│   ├── context.json                # the context projection (variables, artifacts, workspace)
│   ├── pending_input.json          # the question a paused execution is waiting on
│   ├── events/{sequence:08}.json   # append-only event log
│   └── artifacts/{artifact_id}/    # files produced by actions
├── tasks/{execution_id}/
│   └── code/
│       ├── {primary_repository}/   # writable
│       └── {secondary_repository}/ # read-only
└── orchestrator.log                # only written by `tui`, and only with RUST_LOG set
```

Every write of `state.json` and `context.json` is atomic (temporary file in the
same directory, `fsync`, `rename`), and event numbering increases monotonically
without gaps. An interrupted write leaves the previous version of the file
intact.

---

## What the MVP does **not** do yet

Deliberately deferred to Phase 3 — each item needs a plan of its own:

- **`WaitAction`** and the service that polls for external events.
- **Agent decisions** (`decision: { type: agent }`) — they return an explicit
  "not available" error instead of quietly behaving like a rule-based decision.
- **A REST API and SSE** — the TUI and the CLI talk to the engine in-process.
- **Containerisation (Docker)** — isolation rests today on filesystem
  permissions, which protects against a mistake and not against a malicious
  agent. Do not run untrusted workflows.
- **Authentication, multiple users, a web UI, metrics and distributed
  tracing.**
