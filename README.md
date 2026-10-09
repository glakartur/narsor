# Narsor

A workflow engine for tasks carried out by AI agents and by people. A process is
described in YAML (a graph of states, actions and transitions) and the
narsor runs it step by step, persisting every state and every event to
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
- **`git` on `PATH` — a runtime prerequisite.** The narsor clones
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
cargo build              # builds target/debug/narsor
cargo test               # the whole suite (unit + integration)
cargo build --release    # release build
```

The integration tests run against a real filesystem and spawn real child
processes — always in temporary directories, never inside the repository.

---

## Quick start

```bash
# 1. Check the definition
narsor validate tests/fixtures/cli/greet.yaml

# 2. Run the workflow
narsor run tests/fixtures/cli/greet.yaml \
    --input "greeting=Good morning" \
    --input "script_path=$PWD/tests/fixtures/cli/announce.sh"

# 3. See what is going on
narsor list
narsor show <EXECUTION_ID>

# 4. Answer the question the workflow asked
narsor input <EXECUTION_ID> --field approval_status=approved

# …or drive all of that from the full-screen interface
narsor tui
```

---

## Global options

| Option | Default | Meaning |
|---|---|---|
| `--root <DIRECTORY>` | `./.narsor` | The data directory: executions and workspaces. Applies to every subcommand, `tui` included. |
| `--config <FILE>` | searched for | The configuration file to read instead of searching. A path that is not there is a usage error, not a fall-through. |
| `--server <URL>` | `NARSOR_SERVER`, else the local daemon | The control plane for `task`, `backend`, `auth`, `controller` and `tui --remote`: `https://` (plain `http://` only for loopback). See [Remote tasks](#remote-tasks-and-backends). |
| `--token-file <FILE>`, `--token-command <CMD>` | `NARSOR_TOKEN_FILE`, `NARSOR_TOKEN_COMMAND`, `NARSOR_TOKEN` | Where the OIDC token for `--server` comes from; a file or command is read again after a `401`. Never a command-line value. |
| `--help`, `--version` | — | Help and version. |

`tracing` logs go to **stderr** and the output of the command to **stdout**, so
that `narsor list | grep paused` keeps working with `RUST_LOG=debug` set.
The exception is `tui`, which owns the whole terminal: see
[`tui`](#tui) below.

---

## Subcommands

### `validate <FILE>`

Static validation of a definition against `WORKFLOW_SCHEMA.md` §5: graph
consistency, well-formed actions and decisions, references to variables. It
reports **every** violation at once, not just the first one it meets.

```console
$ narsor validate tests/fixtures/implement_feature.yaml
The definition is valid: implement-feature (version 1), states: 9, initial state: analyze.

$ narsor validate tests/fixtures/invalid/five_violations.yaml
Definition 'tests/fixtures/invalid/five_violations.yaml' is invalid — 5 violations found:
  - ...
```

### `run <FILE> [--input NAME=VALUE]…`

Durably queues a task, then stays attached while the scheduler admits it,
the backend provisions its workspace, and its execution reaches a stopping
point or fails. The task id is acknowledged first; an execution id is printed
only after the admitted runner has created a real execution.

`--input` values are parsed **according to the type declared in the
definition**: `string` and `repository` as text, `integer` as a whole number,
`boolean` as `true`/`false`. That is why `--input is_experimental=true` reaches
the context as a boolean and not as a string. Missing required inputs are
reported **before** anything is written to disk.

```console
$ narsor run tests/fixtures/cli/greet.yaml \
      --input "greeting=Good morning" \
      --input "script_path=$PWD/tests/fixtures/cli/announce.sh" \
      --input is_experimental=true
Queued task: task-…
Created execution: 4f1c8f0e-...-9b21
Human input expected: Is the announcement all right?
  --field approval_status=…  (Select, required, allowed values: approved, rejected)
Status: input_needed
```

**Where the workspace comes from.** In this order:

1. `globals.workspace: { type: git, repository: …, ref: … }` — the `repository`
   field may hold a `{{ inputs.* }}` template, which is expanded before use; the
   result is then resolved **registry first** (see [`repo`](#repo)): a value
   that names a registered repository is cloned from that entry's remote, and
   anything else is handed to `git clone` as a raw remote (a URL or a local
   path).
2. `globals.workspace: { type: local, path: … }` — the contents of that
   directory are copied into the workspace. The path must be absolute and, once
   symlinks and `..` are resolved, lie inside a directory listed in the
   `local_workspace_roots` setting (a list; `NARSOR_LOCAL_WORKSPACE_ROOTS`,
   comma-separated). **It is empty by default, which refuses every local
   directory** — and the same applies to a `type: git` repository that is a
   plain local path. Symlinks are copied as links; one that leads outside the
   directory fails the task's provisioning instead of copying its target.
3. No `workspace` section — an empty runner workspace is created after
   admission. That is enough for
   workflows whose scripts live outside the repository.

Everything checkable without touching the disk (a well-formed remote, the
existence of a local path) is checked before the task is queued. The clone or
copy happens only after scheduler admission, in storage owned by that task's
backend. If provisioning fails before an execution exists, the task records
the failure; no synthetic execution is created.

The admitted runner uses the sealed workflow snapshot stored with the task,
not a mutable definition path, so later `input` and `resume` do not need the
original file.

### `list`

Lists the executions in the data directory, newest first. An empty store is not
an error.

```console
$ narsor list
ID                                     STATUS     STATE                  WORKFLOW
4f1c8f0e-...-9b21                      paused     approve                cli-greet
```

### `show <EXECUTION_ID>`

The status, the current state, the workspace and the context variables; plus the
question and its form fields, when the execution is waiting on a human. When the
conductor has something to add, `show` prints it too: the problem a `failed`
execution is stopped on (the conductor's diagnosis, or the raw error) with the
failure's key and the `recover` command to answer it, the conductor's summary
and recommendation of the `human` gate the execution waits on, and what the
conductor is working on right now. `run`, `input`, `resume` and `recover` print
the same when they return.

> **Known limitation.** Executions started through the task scheduler run in a
> separate runner process. The conductor (summaries, chat, proposals) and
> `recover`/`resume` through the legacy execution routes are not yet wired to
> them; only executions that predate the scheduler have them.

```console
$ narsor show 4f1c8f0e-...-9b21
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
$ narsor events 4f1c8f0e-...-9b21
    1  2026-09-23T19:43:49+00:00  workflow_started       Workflow 'cli-greet' started
    2  2026-09-23T19:43:49+00:00  state_entered          Entered state 'announce'
    3  2026-09-23T19:43:49+00:00  action_started         Action 'script' started
```

### `steps <EXECUTION_ID>`

The step timeline: one line per action attempt, computed from the event log
alone (not stored separately) — the CLI counterpart of the TUI's task view
axis. A state visited twice, or retried once, produces two or three lines,
never one line mutated in place: `<state> #n` counts attempts of that state
across the whole execution, so a retried step shows up as `plan #1` (failed)
followed by `plan #2` (succeeded), and a loop's second visit continues the
count rather than restarting it.

Each line names the label, the status (`succeeded`, `failed`, `interrupted`,
`in_progress`, `awaiting_decision`), the action type, and when the attempt
started and ended; below it, only when they apply: the error, a note that a
retry is still pending, that the harness session was interrupted even though
the attempt still produced a label, which harness session it resumed, and —
for an agent step — whether its conversation was recorded, never created (with
why), or is unavailable because the log predates conversation records.

```console
$ narsor steps 4f1c8f0e-...-9b21
plan #1        failed             agent      2026-09-26T10:00:00+00:00  ->  2026-09-26T10:00:01+00:00
    error: harness exited without a result
plan #2        succeeded          agent      2026-09-26T10:00:02+00:00  ->  2026-09-26T10:00:20+00:00
```

### `conversation <EXECUTION_ID> <ATTEMPT_ID>`

One agent attempt's conversation, in order, in full — nothing is truncated
(unlike the `AgentMessage` event, which is). `ATTEMPT_ID` is the attempt's
`action_execution_id`, as printed by `steps` or `events`. Every entry is
classified the same way the TUI's task view classifies it (user, assistant,
thinking, tool call, tool result, the harness's final result, system,
`stderr`, or a narsor-written line — the prompt sent, or the attempt's
outcome) and printed with its full decoded content; an entry this module does
not recognize the shape of still prints in full, tagged `raw`. An attempt with
no conversation record — a script step, an agent step whose conversation
never started, or a step from a log written before this feature existed — is
a readable failure, not silent empty output.

```console
$ narsor conversation 4f1c8f0e-...-9b21 3f9c2e10-...-77aa
    1  2026-09-26T10:00:02+00:00  narsor     prompt: implement the feature…
    2  2026-09-26T10:00:03+00:00  assistant  I'll start by reading the existing tests.
    3  2026-09-26T10:00:04+00:00  tool call  {"name":"bash","input":{"command":"cargo test"}}
    4  2026-09-26T10:00:05+00:00  result     {"subtype":"success","result":"done"}
```

### `input <EXECUTION_ID> [--field NAME=VALUE]… [--label LABEL] [--data NAME=VALUE]… [--variable NAME=VALUE]… [--comment TEXT]`

Delivers a human answer to an `input_needed` execution and resumes it. `--field` fills in
the form fields declared by the `human` action; `--label` names the output label
explicitly — needed when the action has no fields, or when it has several and
the label cannot be derived from a single `select` field. `--file` points at the
definition when the file has moved.

For an agent's `awaiting_input` question, submit `--field answer=...`; no
`--label` is needed because the answer resumes the same agent action rather
than selecting a transition. An agent may additionally return an `options`
array: send one with `--field option=...`, optionally with `--field comment=...`.

An agent step whose answer carried no usable output label — even after the
engine asked it once more — waits as a **problem**: you supply the output the
agent did not give. Pick it with `--label`, and give the step's `data` and
context `variables` with `--data` and `--variable` (a value that reads as JSON,
such as `3` or `true`, is taken as that; anything else is text). The request
lists each label with its description and the keys it needs: a label leading
to a `human` gate needs `--data question=...`, and one leading to a state whose
templates use `{{ context.NAME }}` needs `--variable NAME=...` unless the
context already holds it. `--comment` is kept in the event log with the answer.

The answer is bound to the request `input` read: if the request was answered or
closed in the meantime (another window, a transition), it is refused rather
than applied to whatever the execution waits on next.

```console
$ narsor input 4f1c8f0e-...-9b21 --label implemented --data units_completed=3
Input delivered to execution 4f1c8f0e-...-9b21.
```

```console
$ narsor input 4f1c8f0e-...-9b21 --field approval_status=approved
Input delivered to execution 4f1c8f0e-...-9b21.
Status: completed
Current state: completed
```

### `resume <EXECUTION_ID>`

Resumes an execution that was paused by something **other** than a question for
a human (a manual `pause`, for instance). An execution waiting for an answer
needs `input`, not `resume` — that is the guard against delivering the same
answer twice.

### `recover <EXECUTION_ID> [--comment TEXT]`

Starts the failed step of a `failed` (or, after a daemon restart, `unknown`)
execution afresh — the CLI counterpart of the interface's failure form. The
comment is handed to the step's next attempt. The recovery is bound to the
failure the execution is failed on, the key `show` prints: if somebody acted on
that failure first (another window, the conductor's own retry), it is refused
with a readable message instead of starting the step twice. Like `resume`, it
waits for the execution to stop and exits `1` if it failed again.

```console
$ narsor recover 4f1c8f0e-...-9b21 --comment "The script is back in place."
Recovered execution 4f1c8f0e-...-9b21.
Status: completed
Current state: completed
```

### `conductor chat|history|confirm|reject|undo|status <EXECUTION_ID> …`

The conductor of an execution (see [The conductor](#the-conductor)):
`chat <ID> "<MESSAGE>" [--no-wait]` writes to it and prints its answer and
proposals, `history <ID>` prints the chat, `confirm <ID> <PROPOSAL>` and
`reject <ID> <PROPOSAL>` settle a proposal (by id or by its number in the
history), `undo <ID> <DECISION>` takes back its latest choice of a step's
output, and `status <ID>` prints what it is doing and what it kept: the
invocation under way, the queue, the attempts counted per episode, the latest
failure it took up.

### `config show [--format table|json]`

Every setting, its effective value and where that value came from. See
[Configuration](#configuration) for the layers, the file cascade and the
environment variables.

```bash
narsor config show
narsor config show --format json | jq '.settings.data_dir.source'
```

It creates nothing, needs no writable home and no existing data directory, and
it is the one command that still runs when the configuration file does not hold
up.

### `repo`

The registry of repositories a workflow may name by id. `WORKFLOW_SCHEMA.md`
§3.2 defines `globals.workspace.repository` as *the id of a repository
registered in the system* — this is that system. Registering a repository lets
a definition say `repository: acme-api` instead of embedding a URL that changes
whenever the hosting does.

The registry is one JSON document, `{root}/repositories.json`, so it follows the
global `--root` like everything else. Every write is atomic (temporary file in
the same directory, `fsync`, `rename`), and a root without that file is simply
an empty registry — not an error.

#### `repo add <ID> --remote <URL|PATH> [--name …] [--provider …] [--default-branch …] [--description …]`

Registers a repository. `--remote` is required; `--name` defaults to the id and
`--provider` is inferred from the shape of the remote (`github`, `gitlab`,
`local`, otherwise `git`). The id doubles as the directory name the checkout
lands in, so it has to be usable as one, and the remote is checked with the
workspace provisioner's own rules — a value starting with `-` (which `git`
reads as an option) or using the `ext::` transport (which makes `git` run an
arbitrary command) is refused here rather than halfway through a run.

```console
$ narsor repo add acme-api \
      --remote https://github.com/acme/api.git \
      --name "ACME API" \
      --provider github \
      --default-branch main \
      --description "The public API service."
Registered repository 'acme-api' -> https://github.com/acme/api.git

$ narsor repo add acme-api --remote https://github.com/acme/other.git
Could not register the repository: repository 'acme-api' is already registered in '…/repositories.json'
```

#### `repo list`

Every registered repository, ordered by id. An empty registry is not an error.

```console
$ narsor repo list
ID                       PROVIDER   DEFAULT BRANCH   REMOTE
acme-api                 github     main             https://github.com/acme/api.git
acme-web                 gitlab     -                git@gitlab.com:acme/web.git
```

#### `repo show <ID>`

Every field of one entry. An unknown id is exit code `1`.

```console
$ narsor repo show acme-api
Repository:     acme-api
Name:           ACME API
Provider:       github
Remote:         https://github.com/acme/api.git
Default branch: main
Description:    The public API service.
```

#### `repo remove <ID>`

Drops an entry. Removing an id that was never registered is reported, not
silently accepted — an operator who mistypes an id would otherwise be told the
repository is gone while it is still there.

```console
$ narsor repo remove acme-web
Removed repository 'acme-web'.
```

#### Resolution precedence

When `run` provisions a `type: git` workspace, the expanded `repository` value
is resolved in this order:

1. **A registered id.** The entry's `remote` is cloned, its `default_branch` is
   checked out when the workflow declares no `ref` of its own, and the checkout
   directory is named after the id.
2. **A raw remote.** Anything not found in the registry is handed to `git clone`
   unchanged — a URL or a local path — which is what the CLI did before a
   registry existed. Every workflow that carried a URL keeps working untouched.

`RUST_LOG=debug` says which of the two was taken, on stderr.

```console
$ narsor repo add acme-api --remote /srv/git/api --default-branch main
$ narsor run pipeline.yaml --input repository=acme-api   # clones /srv/git/api
$ narsor run pipeline.yaml --input repository=/srv/git/other   # raw remote
```

### `tui`

The full-screen terminal interface: a `lazygit`-shaped console over the same
engine the CLI drives. It honours the global `--root`, so
`narsor --root /srv/data tui` shows that data directory.

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

Updates come from the daemon's live event stream; the event log is read once,
to backfill the history of a newly selected execution. A stream that lags or
closes is shown as an explicit indicator in the Events panel title rather than
a view that silently stops moving. On top of the stream, the interface re-reads
the daemon on its own: every `tui_refresh_interval` (10 s by default, `none`
turns it off; see [Configuration](#configuration)), and shortly after any event
that changes a status, a pending input, a problem or what the conductor is
doing. A refresh never resets what is on screen: the selection, the cursors,
the follow mode, an open form with what was typed into it and a half-written
chat message all stay.

The header counts what waits for an operator — `2 questions · 1 problem` — and
a question or a problem on a task other than the selected one is announced on
the status bar. A **question** (yellow) is a `human` gate or an agent's
question; a **problem** (red) is a step the engine and the conductor could not
settle: an agent answer without a usable output label, or a step that failed
and stays `failed` with the conductor's report beside it.

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
| Executions panel | `Enter` | open the task view for the selected execution |
| Events panel | `f` | toggle following the stream |
| Events panel | `Enter` | open the event detail window |
| Events panel, detail window | `c` | jump to the conversation of the event's attempt (a message on the status bar if it has none) |
| Panels | `?` | open the key map |
| Panels | `q`, `Esc` | quit |
| Task view | `Tab` | switch focus between the step axis and the step's content |
| Task view | `j` / `↓`, `k` / `↑` | move the focused pane's cursor |
| Task view | `PgUp` / `PgDn` | page the focused pane |
| Task view | `g` / `Home`, `G` / `End` | first / last entry of the focused pane |
| Task view | `r` | reload the step axis and re-read the selected step's content |
| Task view | `Enter`, `space` (content pane) | expand or collapse a conversation entry |
| Task view | `i` | the same input form as on the panels, or the failure form of a failed step |
| Task view | `c` | open the conductor's chat (below the step's content) |
| Task view | `Esc` | back to where the task view was opened from, selection preserved |
| Conductor chat | printable keys, `Backspace`, `Enter` | write a message, send it |
| Conductor chat | `Tab` | switch between the message and the history |
| Conductor chat (history) | `j` / `↓`, `k` / `↑`, `g` / `G` | select an entry |
| Conductor chat (history) | `y` / `Enter`, `n` | confirm, reject the selected proposal |
| Conductor chat (history) | `u` | undo the selected decision (then `y` to confirm, `n` / `Esc` to keep it) |
| Conductor chat (history) | `r` | read the chat again |
| Conductor chat | `Esc` | close the chat; the message being written is kept |
| Failure form | `Tab` / `↓`, `Shift+Tab` / `↑` | next / previous row |
| Failure form | `y` / `Enter`, `n` (on a proposal) | confirm, reject the proposal |
| Failure form | printable keys, `Enter` (on the note) | write the note, restart the step with it |
| Failure form | `Esc` | close |
| Event detail window | `j` / `↓`, `k` / `↑`, `PgUp` / `PgDn`, `g` / `G` | scroll |
| Event detail window | `Esc` | close |
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
through `Engine::deliver_human_input` exactly as `narsor input` does;
choose a declared output of the current state, or — with `o`, and only where the
state declares `allow_manual_override` — **force a transition to an arbitrary
state** after a separate `y`/`n` confirmation. A forced transition is written to
the log so it can be audited: a `DecisionMade` naming the `human_override`
provider and carrying the reason, then `HumanTransitionSelected`, `StateExited`
and `TransitionTaken`. The execution is parked `Paused` at the target state (or
finished, when the target is terminal) and the operator resumes it explicitly.

**Event detail window.** `Enter` on the Events panel opens every field of the
selected event, wrapped and scrollable, with structural values (`inputs`,
`payload`, `input`, `context`, `result`) shown as indented JSON — nothing is
squeezed onto the one line the Events panel shows. An `AgentMessage`'s content
is truncated in the log itself (it is not a place for megabytes of harness
chatter); the window says so and points at `c`. Control characters and ANSI
escapes in any event's content are replaced with visible substitutes before
rendering, the same as everywhere else text reaches the terminal.

**Task view.** `Enter` on an execution, or `c` on an event (from the Events
panel or from its detail window) tied to an agent attempt, opens a full-screen
view: a step axis on the left (one line per action attempt, the same fold
`narsor steps` prints — a retried or revisited state shows every attempt
separately, never one line mutated in place) and, on the right, that step's
content — the attempt's conversation record for an agent step, or its raw
events for anything else. Selecting a step by `c` jumps straight to it; `Enter`
on an execution selects the step currently running, or the last one when
nothing is. Conversation entries are shown by kind (user, assistant, thinking,
tool call, tool result, the harness's final result, system, `stderr`, narsor)
and long ones are collapsed to a few lines by default — `Enter`/`space`
expands or collapses the one under the content cursor. The view is refreshed
with the rest of the interface, and `r` re-reads the step axis and the
selected step's content on demand, including a step that is still running. `Esc` returns to wherever the task view was opened from — the
execution list, the Events panel, or the detail window — with that view's
selection exactly as it was, even if new events arrived while the task view
was open.

The task view, and only the task view, reads through a small embedded,
read-only HTTP API the TUI process starts on `127.0.0.1` at a random port,
guarded by a bearer token generated fresh per process (D52) — the same
boundary a later standalone `narsor serve` (M2) will sit behind, without the
task view changing how it asks for a step axis or a conversation. `narsor
steps` and `narsor conversation` are its CLI equivalents (parity principle):
they read the same underlying functions directly against the store, without
going through that API or needing a TUI running at all.

**Creating things.** Two keys create rather than inspect, and each is the CLI
operation of the same name with a form in front of it — nothing is reachable
here that the command line cannot do.

| Key | Opens | The CLI it stands for |
|-----|-------|------------------------|
| `n` | New task | `narsor run <file> --input k=v` |
| `R` | Repositories | `narsor repo list` / `add` / `remove` |

`R` lists the registry in the same order `narsor repo list` prints. Inside
it, `a` opens a registration form (*id* and *remote* required, plus *name*,
*provider*, *default branch* and *description*), and `d` removes the selected
entry behind a separate `y`/`n` confirmation — `d` alone writes nothing. The
form is checked with the registry's own rules **before** anything is stored, so
an unusable id, a remote starting with `-`, a remote using the `ext::` transport
and an already-registered id are each reported next to the field with the form
left open. Nothing invalid reaches `repositories.json`.

`n` opens the new-task modal: type the path of a definition and press Enter to
validate and load it — *every* violation is listed, the same verdict
`narsor validate` gives, not just the first — then fill in the declared
inputs and press Enter to queue it. The declared `type` decides what a value
becomes: a `boolean` is a `true`/`false` pick list and reaches the context as a
JSON boolean rather than the string `"true"`, an `integer` is parsed, and a
`repository` input is a pick list of the registered ids, so an id already in the
registry is chosen instead of retyped. The queued task is selected immediately;
the view follows its queued, provisioning, and running lifecycle and links to
execution detail only once the runner projects a real execution.

Any definition can be started this way, including one that declares
`globals.workspace`. The interface and `narsor run` seal the same launch
request before queueing it, so workspace resolution is identical whichever
surface created the task; the backend provisions it only after admission.

**The conductor in the interface.** Everything the conductor does is visible
and settled from here (see [The conductor](#the-conductor)):

* while it works on a task, the list row, the details panel and the task
  view's header say what it is doing — `◆ conductor: diagnosing the failure`,
  `conductor: choosing the output after a reprompt`, `conductor: answering the
  chat`;
* `i` on a **problem** shows the agent's answers, the conductor's analysis and
  each output label with its description; the label and the `data`/`variables`
  values the conductor proposed are marked `◆` until edited;
* `i` on a **gate** the conductor summarized shows its summary and
  recommendation above the fields — advisory only, the answer stays yours;
* `i` on a **failed step** opens the failure form: the conductor's diagnosis
  with its confidence and rationale, the actions it proposed (each confirmed
  or rejected there), and a restart of the step with a note for its next
  attempt. The restart names the failure, so it is refused if the execution
  has moved on meanwhile. When the conductor could not answer — twice — the
  form shows the raw error and no analysis;
* `c` in the task view opens the chat: the whole history (it lives in the
  daemon, so it is there after the interface restarts), each proposal with
  its state, each decision with its confidence and rationale. A proposal is
  carried out only when you confirm it; one about a moment the execution has
  moved on from is refused with a message, and the view is read again. The
  conductor's own choice of a step's output can be undone: the confirmation
  shows what it chose, how sure it was and why, and the step becomes a
  problem again — a step after it that already started keeps its side
  effects;
* a failure the conductor takes up is not announced on the status bar as
  `failed`; the problem it raises, or its one restart of the step, is.

**The one thing only the TUI can do** is the forced transition to an arbitrary
state; there is no CLI equivalent for it yet.

**Logging.** `tui` takes over the alternate screen, so a log line on stderr
would be written over the panels. With `RUST_LOG` set, the interface's logs are
appended to `{root}/narsor.log` instead; with `RUST_LOG` unset no
subscriber is installed at all and the records are dropped. Either way nothing
reaches the terminal while the interface is up. Every other subcommand keeps
logging to stderr.

---

## Remote tasks and backends

Besides the local executions above, the daemon is a **control plane** for
remote tasks: each task runs isolated on a registered backend (Kubernetes
first, or the `local` backend), with its own storage, and is created, paused,
resumed and archived through one owner API — clients never talk to a backend
or a runner directly.

```bash
narsor backend default                        # which backend new tasks get (set/reset to change)
narsor task create implement-feature --input ticket=NAR-1 [--backend k8s]
narsor task list                              # queue position (#n), backend, lifecycle, queue reason, staleness
narsor task move <task> --before|--after <other> | --top | --bottom   # reorder a queued task
narsor task show <task>                       # observations with their age, timeline, conversation
narsor task pause|resume|start|archive <task> # archive deletes the workload and its storage
narsor task input <task> --field reason=ok    # answer the input the runner waits for
narsor task interrupt <task>                      # cut short the step the runner works on
narsor task transition <task> <state> [--override --reason <why>]   # manual transition; an override is audited
narsor task restart-step <task> [--note <text>] [--failure-key K]   # run the failed or interrupted step again
narsor task conversation <task> [<attempt>]   # read through the control plane
narsor task watch                             # follow every task's events
narsor tui --remote                           # the remote tasks mode of the interface
```

Without `--server` these use the local daemon (trusted mode). For a network
deployment pass `--server https://…` (or `NARSOR_SERVER`) and an OIDC access
token via `NARSOR_TOKEN_FILE`, `NARSOR_TOKEN_COMMAND` or `NARSOR_TOKEN`; a
fresh installation is claimed once with `narsor auth claim <code>`, and the
owner enrolls controllers with `narsor controller enroll-code <backend>`.

Installation, network mode (OIDC, owner claim, TLS proxy), the backend and
image catalogue, secrets, the Kubernetes manifests, controller enrollment and
credential rotation, lifecycle and scheduling semantics are described in
[`docs/operations/remote-backends.md`](docs/operations/remote-backends.md).

## The conductor

The daemon runs a **conductor** beside every execution: an assistant on the
default harness (the one `agent_harness` names, or the harness picked in the
interface) that reads the task and never writes it. It has one history per
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

**The chat** (`c` in the task view, or `narsor conductor chat|history`) lets
you ask about the task. Whatever it proposes — answering the pending input,
telling the agent something, restarting the failed step, taking a transition,
cancelling — happens only once you confirm it (`narsor conductor confirm`,
`reject`, `undo`). A proposal about a moment the execution has moved on from
is refused (`409`). Only the conductor's latest choice of an output can be
undone, and never on a cancelled execution.

**Read-only, per harness.** On `claude-code` and `codex` the conductor runs in
the task's workspace in the harness's own read-only mode (`plan`,
`--sandbox read-only`). `opencode` has no such mode, so there the conductor
runs in an empty scratch directory and knows the task only from the snapshot
it is given: on opencode, read-only means **isolation from the workspace
directory, not a sandbox**.

---

## `AgentAction` — running an AI agent

A state whose action is `type: agent` runs an AI coding agent in the
execution's workspace and takes its transition from the label the agent
returns.

```yaml
analyze:
  action:
    type: agent
    harness: claude-code          # optional; or: codex, opencode
    model: opus                   # optional, passed through verbatim
    workspace_access: read-only   # optional; default: read-write
    prompt: |
      Analyse the repository for: {{ inputs.feature_description }}.
      Write the plan to plan.md.
    parameters:                   # optional, harness-specific but not access control
      max_turns: 20
    read_only_paths:              # optional, relative to the workspace
      - ../other-repo/src
  outputs: [ready, ambiguous]
  transitions:
    ready: implement
    ambiguous: human_clarification
```

`harness` is **optional**. An action that names none uses the configured
`agent_harness`, and failing that the built-in `claude-code` — so a workflow
without it is portable and still passes `validate`. The other side of that
bargain: its behaviour then depends on the environment, and the same file may
run a different agent on a laptop than in a container. Read the effective value
with `narsor config show`; a workflow meant to run reproducibly should declare
`harness:` explicitly.

`timeout` on an agent state works the same way: declared, it wins; absent, the
configured `agent_timeout` applies. Script and human states are untouched by
that setting — a script state that declares no limit still has none.

An agent that needs a person's answer without changing workflow stage returns
`{"status":"awaiting_input","question":"...","options":["..."]}` rather than an output label.
Narsor leaves the execution on that same agent state with status `input_needed`; the
normal `narsor input <id> --field answer='...'` command resumes that state and
adds the answer to the next agent prompt. Harness, sandbox, permission and tool
failures remain action failures: the execution is `failed` on the current state
and no workflow transition is taken.

**The harness binary is a runtime prerequisite, like `git`.** It is never
bundled and never installed; a missing one is reported as an action error
naming the binary and saying it must be on `PATH`, not as
`No such file or directory`.

### How each harness is invoked

All three are run non-interactively, in the workspace, with
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

codex exec [--model <model>] --sandbox <read-only|workspace-write> \
          --config approval_policy=never --json [resume <thread-id>] <prompt>
```

`--print` / `run` are what make the run a batch run; `json` output is what
carries the session id as well as the text. The flag that takes exactly one
value is always the last one before the prompt, so a prompt can never be read
as a flag's argument.

`workspace_access` is the portable access policy for the action: `read-write`
(the default) or `read-only`. It is translated to each harness's native flags,
so a workflow does not need to know which harness is selected. `parameters`
remain harness-specific for settings unrelated to that policy: `max_turns`,
`append_system_prompt` and `resume_session` for `claude-code`; `agent`, `fork`
and `resume_session` for `opencode`; `approval_policy` (only `never`) and
`resume_session` for `codex`. The former access knobs (`permission_mode`,
`dangerously_skip_permissions`, `auto_approve`, and `sandbox`) are rejected
to prevent a per-harness setting from overriding the portable policy. An
unknown value or a wrong type is an error
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
actually said, in order, prompt included. `narsor conversation <id> <attempt>`
(and the TUI's task view) read it; nothing else in narsor reads it. A conversation
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

## Configuration

Every setting has a value built into the binary, so narsor runs with no
configuration at all. What a setting is worth is decided **per setting**, and
the first of these that provides one wins:

| # | Layer | Source shown as |
|---|-------|-----------------|
| 1 | a command-line flag | `flag (--root)` |
| 2 | an environment variable | `env (NARSOR_DATA_DIR)` |
| 3 | the configuration file | `file (/etc/narsor/config.yaml)` |
| 4 | the value built into the binary | `built-in` |

Per setting, not per file: a file that sets one key does not displace a flag
that sets another.

### Where the file is looked for

One file, found by cascade, first hit wins. The files are **not merged** — a
file that loses is shadowed whole:

| # | Where | Source shown as |
|---|-------|-----------------|
| 1 | the path given with `--config` | `flag` |
| 2 | `$XDG_CONFIG_HOME/narsor/config.yaml` (then `.yml`), else `$HOME/.config/narsor/…` | `user` |
| 3 | `/etc/narsor/config.yaml` (then `.yml`) | `system` |

`/etc/narsor/` is the intended mount point of a Kubernetes ConfigMap. The data
directory is deliberately **not** a step of the cascade: the file cannot live
in the directory it configures.

No file in any of those places is a normal, silent state. An **empty** file is
also valid, and means every value is the built-in one — `config show` then
reports that file as the one in force while every source reads `built-in`,
which looks contradictory and is correct.

A candidate that exists but is not a readable file — a directory mounted over
the path, a symbolic link with no target, a mode that denies the read — stops
the start with a message naming the path and the reason. These are the ways a
ConfigMap arrives broken, and running silently on built-in values while a file
sits right there is the failure this layer exists to prevent.

### The settings

```yaml
# ~/.config/narsor/config.yaml — every key is optional
data_dir: /srv/narsor        # executions, workspaces, the registry
agent_harness: claude-code   # for an agent action that names none itself
agent_timeout: 30m           # for an agent action that declares none; `none` for no limit
log_level: info              # off, error, warn, info, debug, trace
tui_refresh_interval: 10s    # how often the TUI re-reads the daemon; `none` turns it off
```

Every one of them is available as an environment variable too, mechanically:

| Key | Variable | Built-in |
|---|---|---|
| `data_dir` | `NARSOR_DATA_DIR` | `./.narsor` |
| `agent_harness` | `NARSOR_AGENT_HARNESS` | `claude-code` |
| `agent_timeout` | `NARSOR_AGENT_TIMEOUT` | `none` |
| `log_level` | `NARSOR_LOG_LEVEL` | `off` |
| `tui_refresh_interval` | `NARSOR_TUI_REFRESH_INTERVAL` | `10s` |

An **empty** variable counts as unset, because that is how Kubernetes renders
an unset key of a ConfigMap.

`RUST_LOG` beats `NARSOR_LOG_LEVEL`: it takes full `tracing` directives such as
`narsor::engine=debug`, where the setting takes a bare level, and the sharper
instrument should not lose. `config show` names whichever one took effect.

`ORCHESTRATOR_ACTION_ID` exists alongside the `NARSOR_` prefix and is **not** a
setting: it is the idempotency key handed to workflow scripts, so it will not
appear in `config show` and it did not change name.

Secrets are not kept in this file and not handled by it (D16): they come from
the environment, or from a mounted `.env`.

### Seeing what is in force

```bash
narsor config show                  # a table: setting, value, source
narsor config show --format json    # the same, for a script
```

```
SETTING          VALUE                        SOURCE
data_dir         /srv/narsor                  file (/home/you/.config/narsor/config.yaml)
agent_harness    claude-code                  built-in
agent_timeout    1800s                        env (NARSOR_AGENT_TIMEOUT)
log_level        debug                        env (RUST_LOG)
                                              file: /home/you/.config/narsor/config.yaml
                                              shadowed: /etc/narsor/config.yaml
```

Every setting is listed, including the ones nobody touched: the question is
what is in force, not what was changed. The source is the concrete thing — the
path of the file that was actually read, the name of the variable, the name of
the flag.

### When the file does not hold up

A file that exists but is unreadable, malformed, sets an unknown key or gives a
value out of range stops **every** subcommand, with exit code 1 and every fault
reported at once rather than only the first.

`config show` is the single exception. It still prints the whole table, marks
the keys that did not hold up, and exits 1 — a diagnostic tool that stops
working in the situation it exists for is no tool at all. The exception does
not extend to a `--config` path that is not there: that is a usage error (exit
code 2) for every command including this one.

### Coming from an older checkout

The application used to be called `orchestrator`, and the rename was a clean
cut: the old paths are not read as a fallback. If you have data or workflows
under the old names, move them once:

```bash
mv ./.orchestrator ./.narsor
mv ~/.config/orchestrator ~/.config/narsor
```

### What is a contract

The `NARSOR_` prefix, the cascade paths, the `config show` command and the
shape of its JSON are treated as a contract from the first release. The JSON
carries a `version` number so that a later change of shape is a recognisable
change rather than a breakage.

---

## The on-disk layout

Under the `--root` directory (`./.narsor` by default):

```text
{root}/
├── repositories.json               # the repository registry (`repo add` …)
├── executions/{execution_id}/      # executions created before the task scheduler
│   ├── state.json                  # WorkflowExecution: status, current state, context
│   ├── context.json                # the context projection (variables, artifacts, workspace)
│   ├── pending_input.json          # the question an input-needed execution is waiting on
│   ├── definition.yaml             # pinned copy of the workflow definition for this execution
│   ├── checkpoint.bundle           # git bundle of the workspace delta against base_commit
│   ├── checkpoint.git/             # scratch area for materialising the checkpoint
│   ├── events/{sequence:08}.json   # append-only event log
│   ├── conversations/{action_execution_id}.jsonl(.zst)  # per-attempt agent conversation record
│   └── artifacts/{artifact_id}/    # files produced by actions
├── orchestration/                  # durable queued tasks, ordering, leases and priorities
├── local-controller/credential     # the daemon-owned local controller's identity
├── local-controller/storage/{task_id}/   # under `work_dir` (same as {root} by default)
│   ├── state/                      # execution history once the runner initializes it
│   └── workspace/                  # provisioned only after scheduler admission
└── narsor.log                # only written by `tui`, and only with RUST_LOG set
```

Every write of `state.json`, `context.json` and `repositories.json` is atomic
(temporary file in the same directory, `fsync`, `rename`), and event numbering
increases monotonically without gaps. An interrupted write leaves the previous
version of the file intact.

---

## Workflows: built in, by name, overridable

The two shipped workflows — `compound-engineering` and `open-mercato-sdlc` —
are compiled into the binary, so `cargo install --path .` is enough to run them
from anywhere:

```bash
narsor run compound-engineering \
  --input feature_description="rate-limit the public API" \
  --input target_repo=acme-api
```

`run`, `validate` and `--file` take a *workflow reference*: a path, or a name
with or without a `.yaml` / `.yml` suffix. It is resolved in this order, first
hit wins:

| # | Where | Source shown as |
|---|-------|-----------------|
| 1 | an existing file at that exact path | `file` |
| 2 | `{root}/workflows/<name>.yaml` (then `.yml`) | `root` |
| 3 | `$XDG_CONFIG_HOME/narsor/workflows/<name>.yaml`, else `$HOME/.config/narsor/workflows/<name>.yaml` | `user` |
| 4 | the workflow built into the binary | `built-in` |

A path always wins, so `narsor run ./draft.yaml` keeps meaning exactly
that file. A copy in one of the directories **overrides** the built-in of the
same name. When nothing matches, the error lists every place that was tried and
names the built-ins.

### Looking at what is reachable

```bash
narsor workflow list          # every workflow, its source, its description
narsor workflow show NAME     # the YAML that would run (stdout); the source on stderr
```

`list` prints one row per name and marks an override rather than listing the
same name twice:

```
NAME                         SOURCE     DESCRIPTION
compound-engineering         user       The compound engineering chain, end to end: …
                                        overrides: built-in
                                        path: /home/you/.config/narsor/workflows/compound-engineering.yaml
open-mercato-sdlc            built-in   The Open Mercato software delivery process …
```

### Making a built-in your own

```bash
narsor workflow eject compound-engineering        # → the user directory
narsor workflow eject compound-engineering --to ./workflows
```

`eject` writes the built-in out as a file, creating the directory if needed, and
refuses to overwrite an existing one unless you pass `--force`. The copy takes
precedence from that moment on — edit it and every later `run` of that name uses
your version.

### Starting one from the interface

`narsor tui`, then `n`: the modal opens on the same list, each row naming
its source (`built-in`, `user`, `root`, and `overrides: …` where a copy shadows a
built-in). `←` / `→` — and `space`, `j` / `k`, `↑` / `↓` — change the selection,
`Enter` loads and validates it, and the last row, *Enter a path…*, still takes an
arbitrary path for a definition you are writing.

### Resuming what a built-in started

An execution records both the definition's path (when it came from a file) and
its workflow name, in `metadata.cli.definition_path` and
`metadata.cli.definition_name`. A built-in has no path, so the name is what makes
`narsor input` and `narsor resume` — and the interface — find the
definition again in a later process. The path is used first when it is still
there, so executions created before this existed keep resolving unchanged;
`--file <WORKFLOW>` overrides both and accepts a name as well as a path.

---

## Shipped workflows

Two runnable pipelines live in `workflows/`, written against `WORKFLOW_SCHEMA.md`
and modelled on real agentic development processes rather than invented ones:

| File | Process | Modelled on |
|---|---|---|
| `workflows/compound-engineering.yaml` | brainstorm → plan → work → code review → commit/push/PR → compound | the `ce-*` skills of the compound-engineering plugin |
| `workflows/open-mercato-sdlc.yaml` | Discovery → Intake → Triage → Claim → Design → Implement → PR → review loop → QA → Merge → housekeeping | Open Mercato's `SDLC.md` and the `om-*` skills |

```sh
narsor validate open-mercato-sdlc        # by name, from anywhere

narsor repo add acme-web --remote git@github.com:acme/web.git   # once
narsor run compound-engineering \
  --input feature_description="let a reviewer subscribe to a run" \
  --input target_repo=acme-web

narsor show <execution-id>          # where it is waiting
narsor input <execution-id> ...     # answer the gate and resume
```

Neither pipeline is autonomous end to end, and that is the point: both stop for a
person wherever a decision is irreversible, outward-facing, or beyond what an
agent can settle from the repository alone.

**Compound engineering** stops seven times — when the brainstorm finds no
direction it can take; on the scope, before planning; **on the plan, before a
line of code exists**, which is the cheapest correction point in the process;
when `ce-work` hits a genuine ambiguity, so it asks instead of guessing; on what
to do with the code-review findings; before the push and PR, the first
outward-facing act; and on the open pull request, where the review loop turns.

**Open Mercato** stops five times, at the gates `SDLC.md` names itself: the
ticket-level Definition of Ready that only a human can supply (with a
maintainer's waiver as its own option); the design contract for a user-facing
change, settled before the code exists and skipped entirely when the change
touches no UI; the QA sign-off, because "manual means a person judges the result
and owns `qa-approved`", pinned to the tested commit; the squash-merge, the point
of no return; and a human-owned `blocked_gate` that every dead end routes to
rather than failing silently.

Both model the loops the real processes have — review → changes requested →
re-work → re-review, and a QA failure back into implementation — and both are
exercised end to end in `tests/workflows.rs` against a fake harness, with the
human gates answered by script. `workflows/README.md` documents every gate and
what a maintainer editing these files has to keep true.

---

## What the MVP does **not** do yet

Deliberately deferred to Phase 3 — each item needs a plan of its own:

- **`WaitAction`** and the service that polls for external events.
- **Agent decisions** (`decision: { type: agent }`) — they return an explicit
  "not available" error instead of quietly behaving like a rule-based decision.
- **A REST API and SSE for driving executions** (run, input, transitions, a
  live event stream) — the TUI and the CLI still talk to the engine
  in-process for all of that. What exists today is narrower: a read-only HTTP
  API embedded in the TUI process, loopback-only and token-protected (D52),
  serving only the step timeline and conversation reads the task view needs —
  the first step toward D36–D38, not the whole of it.
- **Containerisation (Docker)** — isolation rests today on filesystem
  permissions, which protects against a mistake and not against a malicious
  agent. Do not run untrusted workflows.
- **Authentication, multiple users, a web UI, metrics and distributed
  tracing.**
