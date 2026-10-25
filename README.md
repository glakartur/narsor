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
cargo build --workspace --bins
cargo test --workspace
cargo test --workspace -- --include-ignored  # also process lifecycle tests
cargo build --workspace --bins --release
```

The include-ignored run also exercises three CLI cases that remain marked
ignored because conductor status/chat and problem recovery are not yet wired
to scheduler-admitted runner executions. It therefore currently reports those
three failures; they await the unified task scheduler follow-up. The
live-cluster Kubernetes smoke test also requires its documented environment
variables and a reachable cluster.

The workspace build keeps `narsor`, `narsor-cli`, `narsor-server`,
`narsor-runner`, and `narsor-controller` together in `target/debug` (or
`target/release`), so the client can find the server and the server can find
the local runner. To install all process binaries into the same directory, run
`./install.sh` (add `--root DIR` to install into `DIR/bin`), or by hand:

```bash
cargo install --path . --bins
cargo install --path crates/narsor-server
cargo install --path crates/narsor-runner
cargo install --path crates/narsor-controller
```

The integration tests run against a real filesystem and spawn real child
processes — always in temporary directories, never inside the repository.

---

## Quick start

```bash
# 1. Check the definition
narsor-cli validate tests/fixtures/cli/greet.yaml

# 2. Run the workflow
narsor-cli run tests/fixtures/cli/greet.yaml \
    --input "greeting=Good morning" \
    --input "script_path=$PWD/tests/fixtures/cli/announce.sh"

# 3. See what is going on
narsor-cli list
narsor-cli show <EXECUTION_ID>

# 4. Answer the question the workflow asked
narsor-cli input <EXECUTION_ID> --field approval_status=approved

# …or open the full-screen interface
narsor
```

---

## Global options

| Option | Default | Meaning |
|---|---|---|
| `--root <DIRECTORY>` | `./.narsor` | The data directory: executions and workspaces. Used by the TUI and by CLI commands that need local state. |
| `--config <FILE>` | searched for | The configuration file to read instead of searching. A path that is not there is a usage error, not a fall-through. |
| `--server <URL>` | `NARSOR_SERVER`, else the local server | The control plane for `narsor` and `narsor-cli`: `https://` (plain `http://` only for loopback). See [Remote tasks](#remote-tasks-and-backends). |
| `--token-file <FILE>`, `--token-command <CMD>` | `NARSOR_TOKEN_FILE`, `NARSOR_TOKEN_COMMAND`, `NARSOR_TOKEN` | Where the OIDC token for `--server` comes from; a file or command is read again after a `401`. Never a command-line value. |
| `--help`, `--version` | — | Help and version. |

`tracing` logs go to **stderr** and the output of the command to **stdout**, so
that `narsor-cli list | grep paused` keeps working with `RUST_LOG=debug` set.
The TUI is the `narsor` binary and owns the whole terminal; operator commands
run through `narsor-cli`.

---

## CLI commands (`narsor-cli`)

Task commands use the local `narsor-server` discovered for `--root`, starting
the sibling server when needed. To run the server in the foreground, use
`narsor-server --root <DIRECTORY>`. `narsor-cli daemon stop` sends a graceful
stop signal to the server recorded for that state directory; it does not start
the server. The same lifecycle operation is available as
`narsor-server stop --root <DIRECTORY>`.

### `validate <FILE>`

Static validation of a definition against `WORKFLOW_SCHEMA.md` §5: graph
consistency, well-formed actions and decisions, references to variables. It
reports **every** violation at once, not just the first one it meets.

```console
$ narsor-cli validate tests/fixtures/implement_feature.yaml
The definition is valid: implement-feature (version 1), states: 9, initial state: analyze.

$ narsor-cli validate tests/fixtures/invalid/five_violations.yaml
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
$ narsor-cli run tests/fixtures/cli/greet.yaml \
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
$ narsor-cli list
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
$ narsor-cli show 4f1c8f0e-...-9b21
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
alone (not stored separately) — the CLI counterpart of the *Przebieg* tab of the
TUI. A state visited twice, or retried once, produces two or three lines,
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
$ narsor-cli steps 4f1c8f0e-...-9b21
plan #1        failed             agent      2026-09-26T10:00:00+00:00  ->  2026-09-26T10:00:01+00:00
    error: harness exited without a result
plan #2        succeeded          agent      2026-09-26T10:00:02+00:00  ->  2026-09-26T10:00:20+00:00
```

### `conversation <EXECUTION_ID> <ATTEMPT_ID>`

One agent attempt's conversation, in order, in full — nothing is truncated
(unlike the `AgentMessage` event, which is). `ATTEMPT_ID` is the attempt's
`action_execution_id`, as printed by `steps` or `events`. Every entry is
classified the same way the TUI's *Rozmowa* tab classifies it (user, assistant,
thinking, tool call, tool result, the harness's final result, system,
`stderr`, or a narsor-written line — the prompt sent, or the attempt's
outcome) and printed with its full decoded content; an entry this module does
not recognize the shape of still prints in full, tagged `raw`. An attempt with
no conversation record — a script step, an agent step whose conversation
never started, or a step from a log written before this feature existed — is
a readable failure, not silent empty output.

```console
$ narsor-cli conversation 4f1c8f0e-...-9b21 3f9c2e10-...-77aa
    1  2026-09-26T10:00:02+00:00  narsor     prompt: implement the feature…
    2  2026-09-26T10:00:03+00:00  assistant  I'll start by reading the existing tests.
    3  2026-09-26T10:00:04+00:00  tool call  {"name":"bash","input":{"command":"cargo test"}}
    4  2026-09-26T10:00:05+00:00  result     {"subtype":"success","result":"done"}
```

### `input <EXECUTION_ID> [--field NAME=VALUE]… [--label LABEL] [--artifact NAME=PATH]… [--variable NAME=VALUE]… [--comment TEXT]`

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
agent did not give. Pick it with `--label`, and give the step's artifacts and
context `variables` with `--artifact NAME=PATH` and `--variable` (a value that
reads as JSON, such as `3` or `true`, is taken as that; anything else is text).
The request lists each label with its description and the names it needs: the
variables and artifacts the choice requires, and any `{{ context.NAME }}` the
next state's templates use that no step declares and the context does not hold
yet. `--comment` is kept in the event log with the answer.

The answer is bound to the request `input` read: if the request was answered or
closed in the meantime (another window, a transition), it is refused rather
than applied to whatever the execution waits on next.

```console
$ narsor-cli input 4f1c8f0e-...-9b21 --label implemented --variable units_completed=3
Input delivered to execution 4f1c8f0e-...-9b21.
```

```console
$ narsor-cli input 4f1c8f0e-...-9b21 --field approval_status=approved
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
$ narsor-cli recover 4f1c8f0e-...-9b21 --comment "The script is back in place."
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
narsor-cli config show
narsor-cli config show --format json | jq '.settings.data_dir.source'
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
$ narsor-cli repo add acme-api \
      --remote https://github.com/acme/api.git \
      --name "ACME API" \
      --provider github \
      --default-branch main \
      --description "The public API service."
Registered repository 'acme-api' -> https://github.com/acme/api.git

$ narsor-cli repo add acme-api --remote https://github.com/acme/other.git
Could not register the repository: repository 'acme-api' is already registered in '…/repositories.json'
```

#### `repo list`

Every registered repository, ordered by id. An empty registry is not an error.

```console
$ narsor-cli repo list
ID                       PROVIDER   DEFAULT BRANCH   REMOTE
acme-api                 github     main             https://github.com/acme/api.git
acme-web                 gitlab     -                git@gitlab.com:acme/web.git
```

#### `repo show <ID>`

Every field of one entry. An unknown id is exit code `1`.

```console
$ narsor-cli repo show acme-api
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
$ narsor-cli repo remove acme-web
Removed repository 'acme-web'.
```

#### Resolution precedence

When `run` provisions a `type: git` workspace, the expanded `repository` value
is resolved in this order:

1. **A registered id.** The entry's `remote` is cloned, its `default_branch` is
   checked out when the workflow declares no `ref` of its own, and the checkout
   directory is named after the id. An explicit `ref` in the workflow wins over
   `default_branch`. The bundled workflows declare no `ref`, so they use the
   branch of the repository. With neither, the clone takes the remote's `HEAD`.
2. **A raw remote.** Anything not found in the registry is handed to `git clone`
   unchanged — a URL or a local path — which is what the CLI did before a
   registry existed. Every workflow that carried a URL keeps working untouched.

`RUST_LOG=debug` says which of the two was taken, on stderr.

```console
$ narsor-cli repo add acme-api --remote /srv/git/api --default-branch main
$ narsor-cli run pipeline.yaml --input repository=acme-api   # clones /srv/git/api
$ narsor-cli run pipeline.yaml --input repository=/srv/git/other   # raw remote
```

### TUI (`narsor`)

The full-screen terminal interface: a `lazygit`-shaped console over the
control plane (`/control/v1`). There is one mode. The backend is the one the
configuration names: the local server of `--root` (started when none runs),
or a remote control plane with `--server` /
`NARSOR_SERVER` and a token (see [Remote tasks](#remote-tasks-and-backends)).
Run `narsor --server https://control.example` to open the TUI against a remote
server. Use `narsor-cli --server https://control.example ...` for CLI commands.

```text
narsor · local ● live             harness claude-code (konfiguracja) · sloty 2/3 · ? 1 · ! 1
╭▸ Zadania ────────────────╮╭ Zmiana API zamówień ─────────────────────────────────────────╮
│    ●  NAR-14 Edycja pars…││ Zmiana API zamówień · NAR-12                    implement v3 │
│       edit               ││ Jira NAR-12 ↗   PR !482 ↗   running · krok 3/5 · 14m         │
│──────────────────────────││ Przebieg  Rozmowa  Zdarzenia  Dyrygent  Info                 │
││ ? ●  Zmiana API zamówień││───────────────────│   Read src/api.rs                        │
││      approval           │││ ● review_api     │   Edit src/api.rs  +12 −3                │
│──────────────────────────││───────────────────│ │ assistant: Zmieniam sygnaturę…         │
│  ! ✗  Migracja bazy dany…││╭ ? Czeka na Ciebie: approval · 3m ──────────────────────────╮│
│       failed             │││ the answer panel                                           ││
│                          ││╰────────────────────────────────────────────────────────────╯│
│                          ││                                                              │
╰──────────────────────────╯╰──────────────────────────────────────────────────────────────╯
Enter szczegóły  Space następny czekający  . akcje  n nowe  h harness  ?           powiadom.
```

**The screen.** There is no frame around the screen: the first row is the
header — the connection and its state on the left; on the right the
installation's default agent harness (`harness claude-code (konfiguracja)`
when it comes from the configuration, `harness opencode` when the operator
chose it, `harness ?` before the first read), the slots and the counters of
what waits. The last row is the shortcut bar. Every panel between them has a
frame of its own with rounded corners: in the focus colour (cyan, with a bold
title and a `▸` before it) on the panel that has the keys, grey on the others —
so the active panel reads without colour too. The details frame is named after
the task and keeps the focus look while the keys are in the answer panel. New task, history and
repositories use the same frames.

**The list** is the queue: tasks stand in the order of the scheduler (`rank`),
are never grouped or sorted by state, and change place only by an explicit
action (`K` / `J` / `m`, the TUI's `narsor-cli task move`). A row takes two lines.
The first holds the attention mark (`?` a question or an approval, `!` a
problem, a failed step or a failed provisioning), the state icon (`●` running,
`◷` queued, `⟳` provisioning, `‖` paused, `✗` failed or interrupted, `✓` done,
`↻` being sent) and the title from the workflow's `display` (the task id when
there is none) on the whole width of the list. The second, under the title,
is the label: the current step, `#n` in the queue, `offline` when no
controller holds the backend, `nie wysłano`, or the place preview `↕ n/m`
while moving. Every row is two lines even with no label and the rows stack
directly, so the list scrolls by whole tasks. The selection is a bracket,
never a fill: a thin rule across the whole width of the list in a row of its
own directly above the task, another directly below it and a bar beside both
its lines, so the row keeps its own colours. Only the selected task has the
rules, so moving the selection shifts its neighbours by a row. The bracket is
always thin: cyan where the keys are, grey elsewhere (bold and dim without
colour). The history, the steps on *Przebieg* and every list of choices (the
menu, the answer panel, the new task, the repositories) mark their selection
the same way; a conversation and *Zdarzenia* keep only the bar. `z` hides finished tasks as
a filter; `a` archives one. The selection follows the task, not the row,
through every refresh.

**The details** show the header (title and links from `display`, lifecycle,
step, duration) and five tabs: *Przebieg* (the steps and the selected one),
*Rozmowa* (the agent's conversation), *Zdarzenia*, *Dyrygent* (the conductor's
chat and decisions; its status line names the harness the conductor runs on)
and *Info* (the sealed inputs, the observations with their age, the runner and
the controller). `[` / `]` switch the tabs. *Przebieg* has two levels of
focus: the steps, where the details open, and the conversation of the selected
step. `Enter` / `→` on a step that has a conversation to show go into it
(a running step from its first entry on), and there `j` / `k` pick an entry
and the keys of *Rozmowa* work; `Esc` / `←` go back to the same step. While
the keys are in a step's conversation the timeline does not move by itself,
even when following the current step; going back catches up. The history's
details work the same way, without following. A task that has not started says so and where it stands in
the queue. When a running task cannot follow the default harness — its
backend does not run it, or the binary is not on the runner's `PATH` — one
line under the header says so (`opencode niedostępny tutaj — zostaje
claude-code`) and *Info* gives the full reason in the runner's section. It is
information, not a problem: no list mark, no attention count.

**The default harness** (`h` on the list, in the details, the history and the
repositories — never in a text field, a menu or over another overlay) opens a
list: first `domyślny z konfiguracji (<name>)`, which removes the operator's
choice, then the harnesses the daemon supports; the one in force is marked
`●`. `Enter` sends the choice and the header shows `harness <new>…` until the
answer; a refusal comes back as a notification with its cause, the old value
kept. The choice is the installation's, not the task's: new tasks take it, and
tasks already running take it from their next agent action and the
conductor's next invocation (see [`AgentAction`](#agentaction--running-an-ai-agent)).

**What waits for you** is one panel at the bottom of the details, whatever the
kind of waiting: a question or a `human` gate, an approval, a problem, a failed
step (with the conductor's diagnosis and proposals, or the raw error), a
proposal of the conductor's, a failed provisioning. Every choice is a list with
a description of its effect next to it; `Tab` moves between the choices and
their fields, `Enter` sends, `Esc` folds the panel and keeps what was typed.
Action 0, `Napisz do agenta`, is the first choice of every panel: type a
message and send it. The panel keeps the text when the send fails and empties
the field when it succeeds.

When the server offers forms for the task (`TaskSummary.panel`), the panel
shows one form at a time — an answer, the free conversation with the agent, an
action of the workflow, a permission, the entry limit — and `Esc` steps through
them, also from a field; the first line says which one (`1/2 · Esc przełącza`).
The server says which form opens first and when there is none (the agent is at
work). Each form keeps its own draft. The conversation is a text box that grows
to half the height of the details; `Ctrl+J` makes a new line, `Enter` asks
before it sends. Such a panel is not folded by `Esc`; `Tab` leaves it.
`Space` on the list jumps to the next task that waits, `[` / `]` move between
several waitings of one task. The header counts them (`? 1 · ! 1`).

**The action menu** (`.`) lists everything that can be done with the selected
task, each with its shortcut, and greys out what cannot be done with the
reason. What can be done comes from the server (it checks the same rule when
the command arrives); a server that does not say shows "niedostępne: brak
danych". An action marked `!` asks `y` / `n` first.

| Key | Action | The CLI it stands for |
|---|---|---|
| `K` / `J` / `m` | move in the queue (a place preview, `Enter` confirms) | `narsor-cli task move` |
| `p` | pause or resume | `narsor-cli task pause` / `resume` |
| `x` | interrupt the running step (`!`) | `narsor-cli task interrupt` |
| `t` | manual transition; with a reason, an override where the state allows it | `narsor-cli task transition [--override --reason]` |
| `s` | start again an interrupted task or a failed provisioning | `narsor-cli task start` |
| `a` | archive (`!`) | `narsor-cli task archive` |
| `o` | open a link of the task (`display.links`) in the browser | — |
| `c` | action 0, `Napisz do agenta` (first in the menu), in one step: the server picks what it does by the way the task is stopped — a question: a message in the same session; an approval or a permission: a refusal with your note, never an approval; a pause: the note goes to the agent and the task resumes. A text that was not sent stays as a draft of the task | — |
| `c` (*Dyrygent*) | write to the conductor | `narsor-cli task conductor chat` |
| `u` (*Dyrygent*) | undo a decision of the conductor (`!`) | `narsor-cli task conductor undo` |
| the panel | confirm / reject a proposal, restart a failed step with a note | `narsor-cli task conductor confirm` / `reject`, `narsor-cli task restart-step` |
| `n` | new task (a catalogue workflow, or the path of a definition) | `narsor-cli task create <workflow>`, `narsor-cli task create --file <path>`; the catalogue is `narsor-cli task workflows`, the check of a file `narsor-cli task validate <path>` |
| `i` / the panel | answer what waits: fields, `artifacts`, `variables`, label, comment | `narsor-cli task input <task> --field … --artifact … --variable … [--waiting-id ID]` |
| `Space` | the next task that waits | `narsor-cli task show` ends with `Next       waiting: <task>` |
| `H` | history: archived tasks, and executions from before the scheduler (`w` recovers a failed one) | `narsor-cli task list --archived`; `narsor-cli task history list` / `show` / `events` / `steps` / `conversation` / `conductor` / `recover` |
| `R` | repositories: `a` registers, `d` removes (`!`) | `narsor-cli task repo list` / `add` / `remove` (the same registry on any server; `narsor-cli repo …` is the local one) |
| `h` | the installation's default agent harness: choose one, or go back to the configured one | `narsor-cli harness default set <harness>` / `reset` (`narsor-cli harness default` shows it) |
| the header | what the connection offers | `narsor-cli task capabilities` |

The CLI of the remote-aware commands has a machine form too: `narsor-cli task list
--json` and `narsor-cli task show <task> --json` print the task summaries of the
control plane as JSON (text an agent or a runner wrote is cleaned of what a
terminal would act on). A workflow file, a file validation and the old
executions of the history belong to the local daemon: any other server refuses
them with the reason `narsor-cli task capabilities` gives, and a repository is
registered there by a remote git URL only.

`n` opens the catalogue of workflows (built-in, user and root ones, with the
source of each and what a copy overrides) with the inputs of the highlighted
one as the form: the declared `type` decides what a value is (`boolean` and
`repository` are choices, `integer` is checked) and a missing required input
is reported next to the field. The last entry takes the path of a definition
you are writing — validated first, every violation listed, as
`narsor-cli validate` does — which only a local server accepts. A new task is a
row at the end of the list (`↻ wysyłanie…`) until the server lists it; one
whose answer was lost is sent again with the same id, and `r` sends one that
did not arrive.

**Connection.** The interface opens even when the server cannot be reached
(`łączę z … ◌`) and goes on trying. The header shows the state of the stream
(`● live`, `◌ ponawiam (12s)`, `✖ brak połączenia`); without a connection the
data is greyed and marked with its age (`stan sprzed 40s`), actions that change
state are blocked with the reason, and drafts and forms stay. The stream always
resumes from the last event applied. Events of a task patch its row at once, the
rest is read again after a short delay, and every `tui_refresh_interval` (10 s
by default, `none` turns it off; see [Configuration](#configuration)). A runner
report that changes no state, question or attention does not start a read, so the
current step of a task refreshes every `tui_refresh_interval`. A read
never changes the selection, the focus, the tab, the cursors, the following of
the current step or a draft.

**Keys.** One key map serves the handling, the shortcut bar and the help under
`?`, and one key means one thing in a context.

| Where | Keys |
|---|---|
| Everywhere | `?` help, `q` quit (asks when a draft would be lost) |
| List | `j` / `k` (`↓` / `↑`), `g` / `G`, `Space` next waiting, `Enter` details, `.` menu, `n` `z` `H` `R`, `K` `J` `m`, `p` `x` `t` `s` `a` `o` `c` `r`, `h` default harness |
| Details | `[` / `]` tabs, `Tab` content ↔ panel, `.` menu, `h` default harness (the history's details: `[` / `]`, `w` recover, `h`); then by where the keys are: |
| — *Przebieg*, the steps | `Esc` list, `j` / `k` (`↓` / `↑`) `g` / `G` pick a step, `Enter` / `→` the step's conversation, `f` follow the current step |
| — *Przebieg*, a step's conversation | `Esc` / `←` back to the steps, `j` / `k` `PgUp` / `PgDn` `g` / `G` pick and scroll, `Enter` / `Space` expand an entry, `v` raw entry, `Ctrl+V` raw all |
| — the other tabs | `Esc` list, `j` / `k` `PgUp` / `PgDn` `g` / `G` scroll, `Enter` / `Space` expand an entry, `v` raw entry, `Ctrl+V` raw all, `f` to the end and follow |
| Panel | `Tab` actions ↔ fields, `Enter` send, `Ctrl+J` new line (also `Shift+Enter` / `Alt+Enter` where the terminal reports modifiers), `Esc` fold (with the server's forms: next form), `1`–`9` choose, `[` / `]` previous / next waiting (not with the server's forms) |
| Menu | `j` / `k`, `1`–`9`, `Enter`, `Esc` and the shortcuts above |
| Confirmation | `y` confirm, `n` / `Esc` take it back |
| Forms | `Tab` / `Shift+Tab`, `←` / `→`, `Enter` send, `Ctrl+J` new line, `Esc` back (from a new-task field to the list, then close; what was typed stays) |

Colour means something: yellow what needs you, red what is wrong or cannot be
undone, green what happens now, grey what is done or unavailable, cyan focus.
Only the 16 ANSI colours are used, so the interface follows the terminal's
theme; with `NO_COLOR` set it uses bold, dim and reverse only, and every state
is still told by its icon and its word.

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
narsor-cli backend default                        # which backend new tasks get (set/reset to change)
narsor-cli backend limit <backend> [get|set <n>|reset]   # concurrent tasks of a backend, changed without a restart (ad hoc, lasts until the daemon restarts)
narsor-cli harness default [--json]               # the default agent harness, for new and running tasks (set/reset to change)
narsor-cli task create implement-feature --input ticket=NAR-1 [--backend k8s]
narsor-cli task create --file ./my-workflow.yaml --input name=value   # a workflow file of the local daemon, checked first
narsor-cli task list [--archived] [--json]        # queue position (#n), `?`/`!` attention mark, backend, lifecycle, title; --archived: the history
narsor-cli task move <task> --before|--after <other> | --top | --bottom   # reorder a queued task
narsor-cli task show <task> [--json]              # title and links, what it waits on, observations with their age, timeline, conversation
narsor-cli task pause|resume|start|archive <task> # archive deletes the workload and its storage
narsor-cli task input <task> --field reason=ok [--artifact name=path] [--variable k=v] [--label L] [--waiting-id ID]   # answer the input the runner waits for
narsor-cli task interrupt <task>                      # cut short the step the runner works on
narsor-cli task transition <task> <state> [--override --reason <why>]   # manual transition; an override is audited
narsor-cli task restart-step <task> [--note <text>] [--failure-key K]   # run the failed or interrupted step again
narsor-cli task conversation <task> [<attempt>]   # read through the control plane
narsor-cli task watch                             # follow every task's events
narsor-cli task history list|show|events|steps|conversation|conductor|recover <execution>   # executions from before the scheduler (local daemon)
narsor-cli task repo list|add|remove              # the control plane's repository registry
narsor-cli task workflows | validate <path> | capabilities   # the catalogue, a file's violations, what the server offers
narsor [--server URL]                         # the TUI over the same control plane
```

Without `--server` these use the local daemon (trusted mode). For a network
deployment pass `--server https://…` (or `NARSOR_SERVER`) and an OIDC access
token via `NARSOR_TOKEN_FILE`, `NARSOR_TOKEN_COMMAND` or `NARSOR_TOKEN`; a
fresh installation is claimed once with `narsor-cli auth claim <code>`, and the
owner enrolls controllers with `narsor-cli controller enroll-code <backend>`.

Installation, network mode (OIDC, owner claim, TLS proxy), the backend and
image catalogue, secrets, the Kubernetes manifests, controller enrollment and
credential rotation, lifecycle and scheduling semantics are described in
[`docs/operations/remote-backends.md`](docs/operations/remote-backends.md).

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

**Which login a harness uses.** A harness keeps its subscription login next
to its sessions — `codex` in `~/.codex/auth.json`, Claude Code in
`~/.claude/.credentials.json` — so which `HOME` the runner gives a task decides
whether the harness is logged in. A local task keeps the daemon's own `HOME` by
default (`local_home: host`), with the same logins as the console. `local_home:
private` gives every local task a fresh `HOME` on its own storage instead;
`local_codex_home` and `local_claude_config_dir` (absolute paths, exported as
`CODEX_HOME` and `CLAUDE_CONFIG_DIR`) then point the harnesses at a login
directory of your choice. `local_claude_profile_source` (an absolute path,
empty by default) names a Claude Code profile the runner merges into
Claude's configuration directory before every start; it copies nothing when
it is empty or the configuration directory itself, and it needs either
`local_claude_config_dir` or `local_home: private` (then
`$HOME/.claude` on the task storage). The runner reads these on every start — they are
the backend's settings, not part of a task — so switching them leaves a
running task's earlier sessions behind. Inherited `ANTHROPIC_API_KEY`,
`OPENAI_API_KEY` and `CODEX_API_KEY` are always dropped, so a harness never
switches to API billing behind the subscription's back; a task that should
use a key gets it as an `env` secret.

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
{ "choice": "ready", "reason": "the plan covers every requirement", "variables": { "summary": "3 units" }, "artifacts": { "plan_doc": "plan.md" } }
```

`choice` must be one of the state's declared `choices`, and `reason` is one
line that explains it (asked for, but not enforced). `variables` reach the
context when the step declares them, named `artifacts` are registered, and
`additional_data` / `additional_artifacts` carry anything else — exactly as for
a script action. See WORKFLOW_SCHEMA.md §3.3.A for the full contract.

Agents do not reliably emit clean JSON, so the answer is read robustly: a bare
object, a fenced `json` code block, or an object trailing any amount of prose all
work, and the **last** object carrying a `choice` key wins. But **when no
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
agent_harness: claude-code   # for an agent action that names none itself (unless `narsor-cli harness default set` overrides it)
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
narsor-cli config show                  # a table: setting, value, source
narsor-cli config show --format json    # the same, for a script
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
├── orchestration/                  # durable queued tasks, their order (rank), leases
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
are compiled into `narsor-cli`; install the workspace process binaries as
shown under [Building and testing](#building-and-testing) to run them from
anywhere:

```bash
narsor-cli run compound-engineering \
  --input description="rate-limit the public API" \
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

A path always wins, so `narsor-cli run ./draft.yaml` keeps meaning exactly
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

`narsor`, then `n`: the catalogue opens as a list, each row naming its
source (`built-in`, `user`, `root`, and `overrides: …` where a copy shadows a
built-in). `j` / `k` change the selection and the inputs of the highlighted
workflow are described beside it. `Enter` on a workflow goes into its inputs and
the list folds to a strip with the workflow's name: the short inputs (`string`,
`integer`, `boolean`, `repository`) make a bar, and each `text` input is a
multi-line field — the one with the focus takes the rest of the screen, the
others fold to their first lines. `Enter` queues the task, `Ctrl+J` starts a new
line, `Esc` or `Shift+Tab` from the first field returns to the list with what
was typed kept. A workflow without a `text` input gets the bar and nothing else. The last row, *Enter
a path…*, takes an arbitrary path for a definition you are writing (a local
daemon only; the path is validated first and every violation is listed).

### Resuming what a built-in started

An execution records both the definition's path (when it came from a file) and
its workflow name, in `metadata.cli.definition_path` and
`metadata.cli.definition_name`. A built-in has no path, so the name is what makes
`narsor-cli input` and `narsor-cli resume` — and the interface — find the
definition again in a later process. The path is used first when it is still
there, so executions created before this existed keep resolving unchanged;
`--file <WORKFLOW>` overrides both and accepts a name as well as a path.

---

## Shipped workflows

Two runnable pipelines live in `workflows/`, written against `WORKFLOW_SCHEMA.md`
and modelled on real agentic development processes rather than invented ones:

| File | Process | Modelled on |
|---|---|---|
| `workflows/compound-engineering.yaml` | five stages on one Draft merge request — requirements → plan → implementation → learning → finalize (the merge request leaves Draft; a person merges it), with a person at the end of each stage | the `ce-*` skills of the compound-engineering plugin, and `gh` or `glab` |
| `workflows/open-mercato-sdlc.yaml` | Discovery → Intake → Triage → Claim → Design → Implement → PR → review loop → QA → Merge → housekeeping | Open Mercato's `SDLC.md` and the `om-*` skills |

```sh
narsor-cli validate open-mercato-sdlc        # by name, from anywhere

narsor-cli repo add acme-web --remote git@github.com:acme/web.git   # once
narsor-cli run compound-engineering \
  --input description="let a reviewer subscribe to a run" \
  --input target_repo=acme-web

narsor-cli show <execution-id>          # where it is waiting
narsor-cli input <execution-id> ...     # answer the gate and resume
```

Neither pipeline is autonomous end to end, and that is the point: both stop for a
person wherever a decision is irreversible, outward-facing, or beyond what an
agent can settle from the repository alone.

**Compound engineering** opens a PR for the reviewed requirements, then pushes
the plan, implementation and learning to that same PR. After every push it
handles existing PR feedback: fixes are pushed for another review, while no
actionable feedback advances the workflow. A person reviews the scope, plan,
implementation and final learning on the current PR, and at each gate checks
the feedback, accepts the stage at once, or sends it back; implementation can
pause to ask for missing input. Finalize only takes the PR out of Draft; a
person merges it. Before pushing implementation, it runs the target
repository's optional `scripts/verify.sh`; a failure returns to repair and
review, while a missing script skips the check.

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
- **Containerisation (Docker)** — isolation rests today on filesystem
  permissions, which protects against a mistake and not against a malicious
  agent. Do not run untrusted workflows.
- **Authentication, multiple users, a web UI, metrics and distributed
  tracing.**
