# Command-line interface

Reference for the `narsor-cli` command line: global options, the most-used commands, and exit codes. Run `narsor-cli --help` for the full list (also `task`, `workflow`, `backend`, `harness`, `usage`, `auth`, `controller`, `init`, `purge` and `connection-profile`). For the full-screen interface see [the TUI guide](tui.md).

## Global options

| Option | Default | Meaning |
|---|---|---|
| `--root <DIRECTORY>` | `./.narsor` | Sets `global.root`, the base directory of the installation; a relative `server.state` is resolved against it (see [Configuration](configuration.md#directories-relative-to-the-root)). Used by the TUI and by CLI commands that need local state. |
| `--config <FILE>` | searched for | The configuration file to read instead of searching. A path that is not there is a usage error, not a fall-through. |
| `--server <URL>` | `NARSOR_SERVER`, else the local server | The control plane for `narsor` and `narsor-cli`: `https://` (plain `http://` only for loopback). See [Remote tasks](remote.md#remote-tasks-and-backends). |
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

### `validate <WORKFLOW>`

`WORKFLOW` is a YAML file, or the name of a workflow in the server's
catalogue.

Static validation of a definition against [`workflow-schema.md`](../dev/workflow-schema.md) §5: graph
consistency, well-formed actions and decisions, references to variables. It
reports **every** violation at once, not just the first one it meets.

```console
$ narsor-cli validate tests/fixtures/implement_feature.yaml
The definition is valid: implement-feature (version 1), states: 9, initial state: analyze.

$ narsor-cli validate tests/fixtures/invalid/five_violations.yaml
Definition 'tests/fixtures/invalid/five_violations.yaml' is invalid — 5 violations found:
  - ...
```

### `run <WORKFLOW> [--input NAME=VALUE]…`

Durably queues a task, then stays attached while the scheduler admits it,
the backend provisions its workspace, and its execution reaches a stopping
point or fails. The task id is acknowledged first; an execution id is printed
only after the admitted runner has created a real execution.

`--input` values are parsed **according to the type declared in the
definition**: `string` and `text` as text (the legacy type `repository` reads
as `string`), `integer` as a whole number,
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
   `server.local_workspace_roots` setting (a list;
   `NARSOR_SERVER__LOCAL_WORKSPACE_ROOTS`, comma-separated). **It is empty by
   default, which refuses every local directory** — and the same applies to a
   `type: git` repository that is a
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
not a mutable definition path, so a later `input` does not need the original
file.

### `list`

Lists the executions from the control plane's task records, newest first. An empty store is not
an error.

```console
$ narsor-cli list
ID                                     STATUS     STATE                  WORKFLOW
4f1c8f0e-...-9b21                      paused     approve                cli-greet
```

### `show <EXECUTION_ID> [--details]`

The status, the current state and the start and end times, as the execution's
task record holds them; plus the question and its form fields, when the
execution is waiting on a human. `show` reads only the task record: it is fast
and never starts a runner.

`--details` also prints what only the task's storage holds, read through the
task's runner: the context variables, the problem a `failed` execution is
stopped on (the conductor's diagnosis, or the raw error) with the failure's key
and the `narsor-cli task restart-step` command to answer it, the conductor's
summary and recommendation of the `human` gate the execution waits on, and what
the conductor is working on right now. For a stopped task, the control plane
starts a read-only runner for the read, and `show --details` waits for it (up
to 30 seconds), like `events` and `conversation`. When the details cannot be
read, `show --details` prints the task record's part and one line that tells
why.

Every execution runs in a runner process under its task. To restart a failed
step, talk to the conductor or change the task, use the `narsor-cli task …`
commands ([Remote backends](remote.md)).

```console
$ narsor-cli show 4f1c8f0e-...-9b21
Execution:     4f1c8f0e-...-9b21
Workflow:      cli-greet (version 1)
Status:        failed
Current state: greet
Started:       2026-09-23T19:43:49.018665031+00:00
Finished:      2026-09-23T19:44:02.101354212+00:00

$ narsor-cli show 4f1c8f0e-...-9b21 --details
Execution:     4f1c8f0e-...-9b21
Workflow:      cli-greet (version 1)
Status:        failed
Current state: greet
Started:       2026-09-23T19:43:49.018665031+00:00
Finished:      2026-09-23T19:44:02.101354212+00:00
Context variables:
  announced = "Good morning"
  greeting = "Good morning"
Problem (failure): the greeting script exited with status 1
  Failure: 7d2e...-a41c
  Restart with: narsor-cli task restart-step <TASK> --failure-key 7d2e...-a41c [--note TEXT]
```

### `events <EXECUTION_ID> [--follow]`

The event history in sequence order: number, timestamp, type, description.
`--follow` keeps appending new events until the execution reaches a terminal
state.

```console
$ narsor-cli events 4f1c8f0e-...-9b21
    1  2026-09-23T19:43:49+00:00  workflow_started       Workflow 'cli-greet' started
    2  2026-09-23T19:43:49+00:00  state_entered          Entered state 'announce'
    3  2026-09-23T19:43:49+00:00  action_started         Action 'script' started
```

### `steps <EXECUTION_ID>`

The step timeline that the task's runner last reported, kept in the task
record: one line per action attempt — the CLI counterpart of the *Przebieg* tab of the
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

One agent attempt's conversation, in order, read through the task's runner.
`ATTEMPT_ID` is the attempt's `action_execution_id`, as printed by `steps` or
`events`. Each conversation element is one line: entry number, timestamp,
category (`sent`, `thinking`, `agent`, `question`, `tool`, `result`,
`technical`, `unclassified`) and the same one-line summary the TUI's
*Rozmowa* tab shows. Lines that narsor wrote (the prompt, the outcome) are
printed whole; an unclassified entry shows the start of its raw line. A line
of a subagent is indented and starts with `↳ [name]`. An attempt with
no conversation record — a script step, an agent step whose conversation
never started, or a step from a log written before this feature existed — is
a readable failure, not silent empty output.

```console
$ narsor-cli conversation 4f1c8f0e-...-9b21 3f9c2e10-...-77aa
    1  2026-09-26T10:00:02+00:00  sent       [Narsor] implement the feature…
    2  2026-09-26T10:00:03+00:00  agent      I'll start by reading the existing tests.
    3  2026-09-26T10:00:04+00:00  tool       Bash cargo test
```

### `input <EXECUTION_ID> [--field NAME=VALUE]… [--label LABEL] [--artifact NAME=PATH]… [--variable NAME=VALUE]… [--comment TEXT]`

Delivers a human answer to an `input_needed` execution and resumes it. `--field` fills in
the form fields declared by the `human` action; `--label` names the output label
explicitly — needed when the action has no fields, or when it has several and
the label cannot be derived from a single `select` field. `--file` is refused:
the runner always uses the workflow sealed with the task. For a Claude Code
permission request, give `--permission deny|allow-exact|allow-group|allow-pattern`
(with `--pattern` for `allow-pattern`, and optionally `--denial-index`).

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

### `config show [--format table|json]`

Every setting of every Narsor program installed next to `narsor-cli`, its
effective value, where that value came from and which programs read it. Each
program checks its own sections; `config show` merges the results and lists a
program that is not installed as `not installed`. See
[Configuration](configuration.md#seeing-what-is-in-force) for the layers, the
file cascade and the environment variables.

```bash
narsor-cli config show
narsor-cli config show --format json | jq '.settings[] | select(.path == "server.state") | .source'
```

It creates nothing, needs no writable home and no existing data directory, and
it is the one command that still runs when the configuration file does not hold
up. It exits 1 when any program reports a problem or cannot be checked.

### `repo`

The registry of repositories a workflow may name by id. [`workflow-schema.md`](../dev/workflow-schema.md)
§3.2 defines `globals.workspace.repository` as *the id of a repository
registered in the system* — this is that system. Registering a repository lets
a definition say `repository: acme-api` instead of embedding a URL that changes
whenever the hosting does.

The registry is a directory of YAML files, one per repository:
`{catalog}/repositories/{id}.yaml` (`server.catalog` defaults to the root). A
catalogue without that directory is an empty registry, not an error. The
commands go through the control plane, so `--server` selects the registry
they change.

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
Could not register the repository: repository 'acme-api' is already registered in '…/repositories/acme-api.yaml'
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

### `queue`

Automations and queues are files of the catalogue (`{catalog}/automations/`
and `{catalog}/queues/`). They create tasks, or serve the decision gates
that wait on their queue. The control plane keeps the schedule;
`narsor-observer-worker`, which the server starts when its configuration
enables it, runs the scripts (see
[installation](../operations/installation.md#run-the-observer-worker)). How
to write an automation and a queue is in [Automations and queues](observers.md).

The `queue` commands are read-only: no command creates, changes, approves,
pauses or deletes a queue or an automation. Change the files in the
catalogue instead. In network mode a user who is not the installation's
owner gets `403`.

| Command | What it does |
|---|---|
| `queue list [--json]` | One line per queue of the catalogue: its mode, its schedule, its automations and the number of its entries; a queue that does not run says why. Then the catalogue files that cannot be used, each with the reason. |
| `queue show QUEUE [--json]` | One queue: its mode and schedule, the workflow of the tasks it creates, the problem of its file, its automations (description, scripts, state, the run in progress, the next run, a `no worker` warning, and the newest errors with the exit code, the entry and the reason) and its entries, oldest first (task, step, episode, waiting time, params, and for each automation how it handled the entry). |

```console
$ narsor-cli queue list
$ narsor-cli queue show mr-review
$ narsor-cli queue show mr-review --json | jq '.entries | length'
```

With `--json`, the commands print the server's whole answer as JSON, as
`task show --json` does: `queue list` prints the queues and the catalogue
problems, `queue show` prints the queue view. Use it in scripts and for
agents.

The commands `observer create`, `update`, `list`, `show`, `queue list`,
`queue show`, `script set`, `script show`, `approve`, `pause`, `resume` and
`delete` are removed with the observers stored in the state (see
[Automations and queues](observers.md#13-update-from-stored-observers)).
`narsor-cli observer …` is an unknown command.

#### Commands for an automation's script

The worker runs a script with `NARSOR_OBSERVER_RUN` (the run's id),
`NARSOR_SERVER` and `NARSOR_TOKEN` (a token of that run only) set. Two task
commands then go to the effect routes of the run. Only a `check` run may
use them; the server refuses them from an `in` or `out` run:

```sh
narsor-cli task create --data "$JSON"
narsor-cli task decide TASK_ID --episode EPISODE --choice CHOICE [--reason TEXT] [--var NAME=VALUE]… [--var-file NAME=PATH]…
narsor-cli task decide TASK_ID --episode EPISODE --data '{"choice": …, "reason": …, "variables": {…}}'
```

- `task create --data JSON` creates a new task on each call. Narsor does not
  remove duplicates: the script records in its state what it already
  reported. The `tasks` of the queue file decide the workflow, repositories,
  backend and profile: in a script, `WORKFLOW`, `--input`, `--backend`,
  `--repository` and the other options of `task create` are a usage error.
  The data is a JSON object of the fields the queue maps to workflow
  inputs (`tasks.mapping`); Narsor refuses a field that is not in the map.
- `task decide TASK_ID --episode EPISODE` answers the gate of the queue
  entry of this run, as a person answers it. `TASK_ID` and `EPISODE` are
  the `task_id` and the `episode` of the entry. Give `--choice` with
  `--reason`, `--var` and `--var-file`, or give `--data` with the whole
  answer, never both. Narsor checks the answer as it checks the answer of a
  person. The first answer of an episode wins; a later one, and an answer
  for an episode that ended, is refused. When the queue has `out` scripts,
  the answer reaches the task when they ended. The task history shows the
  automation, its run and the reason.

Without `NARSOR_OBSERVER_RUN`, `task create --data` and `task decide` are
usage errors (exit code `2`). In a script:

| Answer | Prints | Exit code |
|---|---|---|
| created, answered | the server's line | `0` |
| the run's task limit is reached, the data is refused, the answer is refused (the episode ended, another answer came first, an invalid choice or variables), the run is not a `check` run, the token is not valid | the reason, on stderr | `1` |
| incorrect usage | the reason, on stderr | `2` |

Text that came from a script or a source (answer reasons, stderr) is
printed without control characters.

### `secret set <NAME>`

Stores a token in the token store of the local controller, which tasks in the
Docker work mode take their component secrets from. The value is read from
standard input (a trailing newline is dropped), never from the command line.
Works only next to the local daemon. See
[Local tasks in Docker](local-docker.md#tokens).

### `image refresh`

Asks the local controller to check the versions of its Docker-mode task
images at the next task start, without waiting for
`controller.local.docker.update_interval`. See
[Local tasks in Docker](local-docker.md#updates).

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
