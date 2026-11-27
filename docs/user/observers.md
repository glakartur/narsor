# Automations and queues

An **automation** looks at an external source (Jira, GitHub, GitLab, or any
other source that a shell script can read) and does one of two things in
Narsor:

- **create a task** for each new object that matches a condition (for
  example, a new Jira issue); or
- **serve a decision gate** of a task that waits on its queue: prepare the
  source when the gate starts to wait, answer the gate (for example, choose
  `check_feedback` when the reviewer of a merge request is done), and clean
  up when the gate exits.

A **queue** names the automations that serve it, when they run and what
they get. Automations and queues are files in the catalogue, as workflow
definitions are. The catalogue is the only place of their configuration:
there is no command that creates, approves, pauses or deletes them. A queue
in the catalogue is active; a change of a file applies to the next run.

Narsor has no built-in client for Jira, GitHub or GitLab. Shell scripts
that the catalogue author writes talk to the source. Narsor keeps the
schedule, runs the scripts, limits their effects and records where each
task and each answer came from.

The words *observer* and *observer worker* are still in the names of the
settings and of the program that runs the scripts
(`narsor-observer-worker`). How the server starts the worker is in
[installation](../operations/installation.md#run-the-observer-worker). The
`watch` block of a `decision` gate is in
[the workflow schema](../dev/workflow-schema.md) (§3.3.D).

> **Run only scripts that you trust.** The worker runs the scripts of the
> catalogue as a separate system user on the server's host, with the
> secrets that their automations name. Review every script before it goes
> into the catalogue, also a script that an agent wrote.

## 1. How it works

```text
control plane                      worker                       script
-------------                      ------                       ------
a gate episode enters the queue
  or a tick of the schedule ────▶  claims the run
                                   (worker key)
                                   starts `sh` with the input
                                   on stdin and the run token  ─▶ reads or changes the source
                                                                  narsor-cli task create …
              ◀────────────────────────────────────────────────── narsor-cli task decide …
                                   reads the exit code,        ◀─ prints {"state": …}
                                   stdout and stderr              (only to create tasks)
saves the result ◀──────────────  reports the result
or records an error                (secrets masked)
```

1. The control plane decides when a script runs. The unit of scheduling is
   one **pair**: a queue and one of its automations. Each pair has at most
   one run at a time.
2. The worker (`narsor-observer-worker`, which the server starts) claims a
   run that is due. It waits for one up to 25 seconds, so a script starts
   soon after it is due. The run gets a token that is valid only for this
   run and only until the run's deadline.
3. The worker starts the script with a clean environment (with
   `server.observer.insecure_same_user: true`, with the server's environment, as
   a runner's agent). The script reads
   its input on stdin (section 5).
4. A `check` script makes its effects with `narsor-cli task create` and
   `narsor-cli task decide` (section 6).
5. The worker reports the exit code, stdout and stderr. The control plane
   saves the result, or records an error.

## 2. The catalogue files

```text
{catalog}/automations/{name}/automation.yaml   the definition
{catalog}/automations/{name}/check.sh          required
{catalog}/automations/{name}/in.sh             optional: runs when a gate episode enters the queue
{catalog}/automations/{name}/out.sh            optional: runs when a gate episode leaves the queue
{catalog}/queues/{name}.yaml                   one queue (.yaml or .yml)
```

The directory name is the name of the automation, and the file name is the
name of the queue: 1–64 characters of `A-Z a-z 0-9 . _ -`, not starting
with `.`. A `watch` block of a gate names the queue.

### Automation

```yaml
# {catalog}/automations/gitlab-mr-label/automation.yaml
description: Puts a review label on a merge request while the gate waits
params: [mr_url, choice]                       # the gate params the scripts read
secrets:                                       # names only, never values
  - {name: gitlab-token, env: GITLAB_TOKEN}
```

| Field | Meaning |
|---|---|
| `description` | Required. The queue views show it. |
| `params` | The names of the gate params (`watch.params`) that the scripts read. For the views and the workflow author; Narsor does not check a gate against it. |
| `secrets` | The secrets the worker gives the scripts: `name` is a file in the secrets directory, `env` the variable the script reads (section 10). At most 32. A `value` field is refused: a secret value is never in the catalogue. |

### Queue

```yaml
# {catalog}/queues/mr-review.yaml
automations: [gitlab-mr-label, github-pr-label]
mode: on_episodes                  # or always
schedule: {cron: "*/5 * * * *"}    # or {interval_secs: 300}
condition:                         # optional: any map, given to the scripts as it is
  label: narsor-review
  gitlab: {host: gitlab.example.com}
```

```yaml
# {catalog}/queues/jira-intake.yaml
automations: [jira-new-issues]
mode: always
schedule: {cron: "*/10 * * * *"}
condition: {base_url: "https://jira.example.com", jql: "project = ABC", fields: [key]}
tasks:                             # mode always only: the scripts may create tasks
  workflow_id: triage              # must be in the workflow catalogue
  workflow_version: 2              # optional: the catalogue's version by default
  workspaces: {code: {repository: api}}   # optional, as in a task create request
  backend_id: k8s                  # optional: the default backend by default
  profile: default                 # optional
  mapping: {ticket: key}           # workflow input: field of the task data
  task_limit_per_run: 10           # 1-1000, default 10
  import_backlog: false            # the first run imports what already matches
```

| Field | Meaning |
|---|---|
| `automations` | Required. The automations that serve the queue, each one once. |
| `mode` | Required. `on_episodes` or `always` (section 3). |
| `schedule` | Required. Exactly one of `interval_secs` and `cron` (section 7). |
| `condition` | A map, at most 64 KiB. Each script of the queue gets it as it is. When several automations serve one queue, they share it: give each one a key of its own (the example uses `gitlab` and `github`). |
| `tasks` | Only for `mode: always`. What each task that a script creates runs. The tasks belong to the owner of the installation. |

### Problems

A load never fails as a whole. A file that cannot be used is a **problem**,
and the rest of the catalogue still loads. The queue views show each
problem with its file and the reason (section 9). Narsor refuses, for
example:

- an automation without `automation.yaml` or `check.sh`, with an unknown
  field, or with a secret value;
- a queue that names a missing or invalid automation (the queue does not
  run);
- a schedule that breaks the rules of section 7;
- `tasks` on an `on_episodes` queue, a workflow that is not in the
  catalogue, or a `mapping` input that the workflow does not declare.

A gate that watches a queue which is not in the catalogue (or is a
problem) waits for a person, as a gate without `watch` does.

### The example catalogue

[`examples/catalog/`](../../examples/catalog/) has a catalogue to copy into
`{catalog}/automations/` and `{catalog}/queues/`:

| File | Mode | What it does |
|---|---|---|
| `queues/mr-review.yaml` | `on_episodes` | The review gates of `workflows/compound-engineering-change.yaml`, served by the two automations below |
| `automations/gitlab-mr-label/` | | `in` adds the review label to the GitLab merge request, `check` answers the gate when the reviewer removed it, `out` removes it when the gate exits |
| `automations/github-pr-label/` | | The same for a GitHub pull request |
| `queues/jira-intake.yaml` | `always` | A task for each new Jira issue, served by `jira-new-issues` |
| `automations/jira-new-issues/` | | `check` only: searches Jira with JQL and creates the tasks |

Change the hosts in the queue files before you use them. The header of each
file gives the condition, the gate params, the secrets and how to test it
with a fake source (section 12). An agent can help you write a script: give
it this page and the examples.

## 3. Queues and run modes

| Mode | When `check` runs | Input | Entry and exit scripts |
|---|---|---|---|
| `on_episodes` | Only when the queue has entries: one run for each entry. No entry, no run. | One entry in `entry` | Yes |
| `always` | On each tick of the schedule, also when no task waits. | No `entry` | No |

An entry comes from a workflow: a `decision` step with `by: human` and a
`watch` block adds one entry for each wait episode of the gate. See
[the workflow schema](../dev/workflow-schema.md) (§3.3.D). Several
automations can serve one queue (for example, one for GitLab and one for
GitHub); each script must then recognize its own entries (section 5).

Use `always` for a queue whose automation creates tasks (for example, from
Jira). Only a `check` run of an `on_episodes` queue can answer a gate.

## 4. Entry and exit scripts

`in.sh` and `out.sh` let the queue, not the workflow, handle the source.
In the example, `in` puts the label `narsor-review` on the merge request,
the reviewer removes it, `check` sees that and answers the gate, and `out`
removes the label if it is still there. The workflow has no label step.

```text
the gate starts to wait ─▶ in (each automation that has one) ─▶ the step is in progress
                                    │ failed 3 times                   │ check runs on the schedule
                                    ▼                                  ▼
                           the step shows `failure`         an exit (check, person, operator, restart)
                           (a person can still choose                  │ held
                            an exit or restart the step)               ▼
                                                            out (each automation that has one)
                                                                       │ ended (succeeded, or failed 3 times)
                                                                       ▼
                                                            the exit reaches the task: the next step starts
```

- **Entry.** When a gate episode enters the queue, the control plane runs
  `in` of each automation of the queue that has one, at once, not on the
  schedule. `check` does not run for the entry before every `in` has
  succeeded. A queue whose automations have no `in` is ready at once.
- **Exit.** Every exit of the gate waits until `out` of each automation
  that has one has ended: an answer of `check`, a choice of a person, an
  operator's transition or chosen step, and a restart of the step. The
  first exit of an episode wins; a second one is refused. When every `out`
  has ended, the held exit goes to the task at the runner's next poll, and
  the next step starts. A queue whose automations have no `out` lets the
  exit through at once.
- **Retries.** A script that exits with a code other than `0`, runs past
  its time limit of 120 seconds, or is lost by the worker is a failed
  attempt. Narsor tries it again after 10 seconds and then after 30
  seconds: 3 attempts in all.
- **A failed `in`.** After the third failed attempt, the gate step shows
  the status `failure` and Narsor makes no more attempts. `check` does not
  run for the entry. A person can still choose any exit of the gate (and
  `out` runs for it), or restart the step.
- **A failed `out`.** After the third failed attempt, Narsor records the
  failure in the task history and the exit goes on. A failed `out` never
  stops the task and needs no action.
- **One after the other.** The entry and exit scripts of one task run one
  at a time. `in` of a new episode waits until `out` of the previous
  episode of the task has ended, so a loop back to the same gate never
  finds the old label.
- **Restart.** The action *Uruchom od nowa* of a gate that waits starts the
  step again in a new episode: `out` of the old episode runs, then `in` of
  the new one. A runner of an older version refuses the restart of a
  waiting gate, and the refusal is shown.
- **The task ends at the gate.** When a task is cancelled, archived or
  deleted while it waits at the gate, `out` runs once in the background.
  (The owner archives a task only at a terminal state of its workflow, not
  at a gate; an archive at the gate comes only from the control plane.) It
  does not hold the end of the task; a failure is in the task history.
- **No new run.** Pause, an interrupted runner, auto-resume and a restart of
  the runner keep the episode: `in` and `out` do not run again.
- **Idempotent.** Narsor can run `in` and `out` more than once (a retry, a
  restart, a server stop between the end of the script and the record of
  its result). `out` can find the effect gone (the reviewer removed the
  label). Write both so that a second run changes nothing.
- **A changed catalogue.** When the automation left the queue, or lost its
  `in` or `out` script, before the script ran, Narsor skips it and records
  that in the task history.
- **A file that cannot be used for a moment.** When the file of the queue
  or of the automation is a problem (section 2), for example because an
  editor saved half of it, and Narsor cannot load the queue or the
  automation without it, Narsor does not skip the script. The script
  waits and runs when the file is correct again. Narsor records one
  warning in the task history that names the file, and the task view
  shows the file next to the script. The gate does not move until an
  operator fixes or removes the file. A restart of the step does not help
  in every case: a restart is an exit, so when the queue has `out` scripts
  and its own file is the problem, the restart is held as well.
  Narsor skips the script only when the queue or the automation is really
  gone from the catalogue. A problem that does not stop the queue or the
  automation (for example, a second file `queues/<name>.yml` next to a
  valid `queues/<name>.yaml`) does not hold the script.
- **An exit that does not happen.** When the runner refuses the held exit
  (for example, an operator's transition that is not allowed, or a restart
  that an older runner refuses), or the held answer becomes obsolete, the
  task still waits at the gate. Narsor records that in the task history and
  puts the gate back to `ready` (or to `failure` when an `in` failed):
  `check` runs again, and `in` does not run again. `out` already told the
  source that the gate was left, so the next exit is held again and runs
  `out` again. This is one more reason why `out` must be idempotent. An
  `out` that still runs for the exit that did not happen does not count
  for the next exit: `out` runs again with the new choice when it ends.

The task view shows the `failure` status of a gate step whose `in` failed,
and each attempt of `in` and `out` as an event in the step's history
(*Przebieg*). An attempt is never a new step. `check` runs report nothing
to the task. `narsor-cli task show` prints the same status in a `Gate`
block: the phase, one line for each script (its status, attempts, last
error and next attempt) and the held exit; `--json` has it in
`gate_automation`.

## 5. The script contract

Each script is a POSIX `sh` script. It can use `curl`, `jq`, `narsor-cli`
and the tools of a base system. The worker runs it with `sh`, so it does
not have to be executable, and it cannot read the other files of its
automation: each script must be complete in itself.

| Script | Reads | Result |
|---|---|---|
| `check.sh` of an `always` queue | the condition and its state | prints its new state on stdout; may create tasks |
| `check.sh` of an `on_episodes` queue | one entry | the exit code; may answer the gate of the entry |
| `in.sh`, `out.sh` | one entry (`out`: also how the gate was left) | the exit code |

### Input (stdin)

One JSON document, contract version 3:

```json
{
  "contract_version": 3,
  "phase": "out",
  "queue": "mr-review",
  "automation": "github-pr-label",
  "condition": {"label": "narsor-review", "github": {"host": "github.com"}},
  "state": null,
  "entry": {
    "task_id": "…",
    "episode": "…",
    "episode_started_at": "2026-11-17T10:00:00Z",
    "step": "review_work",
    "params": {"mr_url": "https://github.com/acme/app/pull/17", "choice": "check_feedback"},
    "choices": [
      {"name": "check_feedback", "description": "The review is done.", "requires_variables": []},
      {"name": "revise_plan", "description": null, "requires_variables": ["note"]}
    ],
    "variables": [
      {"name": "note", "type": "text", "description": "What to change in the plan."}
    ]
  },
  "exit": {"choice": "check_feedback", "author": "automation", "run_id": "…"},
  "first_run": false,
  "import_backlog": false
}
```

| Field | Meaning |
|---|---|
| `contract_version` | `3`. Stop with an error when it has a different value. |
| `phase` | The script that runs: `in`, `check` or `out`. |
| `queue`, `automation` | The pair of the run. |
| `condition` | The `condition` of the queue file, as it is (`{}` when it has none). |
| `state` | What the last successful `check` run of the pair printed in `state`. `null` before the first one, and always `null` for an `on_episodes` queue. |
| `entry` | The one queue entry of the run. Only for an `on_episodes` queue; an `always` run has no `entry` field. |
| `entry.task_id`, `entry.episode` | The task that waits and its wait episode. An answer names both. |
| `entry.episode_started_at` | When the control plane added the entry. |
| `entry.step` | The workflow state of the gate. |
| `entry.params` | The rendered `watch.params` of the gate, as text. Narsor does not check them: the script must check each value before it uses it. |
| `entry.choices` | The choices of the gate, in the workflow's order: name, description, and the variables that an answer with this choice must give. |
| `entry.variables` | The variables that an answer may give: name, type, description and, when the workflow restricts them, `options`. |
| `exit` | Only for `out`: how the gate was left. `choice` is the choice (absent when the task ended at the gate). `author` is `person`, `automation` (with the `run_id` of the run that answered), `operator`, `task_end`, or `runner` when the runner left the gate with no exit that Narsor held. |
| `first_run` | `true` until the first `check` run of the pair succeeds. |
| `import_backlog` | The `tasks.import_backlog` of the queue file. |

Version 2 had an `observer` object in place of `queue` and `automation`,
and no `phase` and `exit`. A script of version 2 must be changed before it
runs again.

### Result of a `check` script that creates tasks

The script prints exactly one JSON document on stdout, and nothing else:

```json
{"state": {"last_id": 10350}}
```

- `state` is required. The next run gets it in its input. When the script
  has nothing to keep, it prints `{"state": {}}`.
- Other fields, a second document or a log line after the document make
  the output not valid.

Write all other text to stderr. This includes the output of `narsor-cli`:
send it to stderr with `>&2`.

| The script | Narsor |
|---|---|
| exits with `0` and prints a valid document | Saves the new state and sets the failure count to 0. |
| exits with a different code, prints a document that is not valid, or runs for longer than its time limit | Keeps the old state. The next run sees the same objects again. Records the error and counts one failure. |

Because a failed run keeps the old state, a script must be safe to run
again. Narsor does not remove duplicates: two `task create` calls make two
tasks. The script must remember in its state what it already reported.

> **Risk of a duplicate task.** When a script creates a task and its new
> state is then not saved (the script fails later in the same run, or the
> worker or the control plane stops before the result), the next run starts
> from the old state and can create the same task again. Keep the window
> small: when a task is not created, stop creating tasks and still print
> the state up to the last task you created, as `jira-new-issues` does.
> When the run created no task at all, exit with an error instead: the
> error is then recorded and counted, and a refusal that does not go away
> stops the pair (`error`) after 5 failed runs in a row.

### Result of a `check` script that answers gates

The exit code is the result. Narsor does not read stdout.

| Exit code | Meaning | Narsor |
|---|---|---|
| `0` | The run is done: the script answered the gate, or it has nothing to answer in this run. | Records the time of the run for the entry. |
| any other code, or the script runs for longer than its time limit | An error. | Records the error for the entry. A new error of the same entry replaces the old one. |

No exit code removes an entry. The entry stays on its queue until its gate
exits, and the next round checks it again. An error of an entry is not
counted as a failure of the pair.

When several automations serve one queue, each script must recognize its
own entries. A script that does not recognize an entry (for example, the
URL is on another host) exits with `0` and does nothing. It must not use
its token for that entry. This holds for `in` and `out` too.

### Result of an `in` or `out` script

The exit code is the result: `0` is a success, anything else is a failed
attempt (section 4). Narsor does not read stdout. An `in` or `out` script
cannot create a task or answer a gate.

### Limits

| Limit | Value |
|---|---|
| Time of one `check` run | 5 minutes. Then the worker stops the script and all its processes. |
| Time of one `in` or `out` attempt | 120 seconds |
| A script file | 256 KiB |
| stdout | 1 MiB |
| stderr | 64 KiB (Narsor keeps the start) |
| Data of one task (`--data`) | 64 KiB |
| Params of one entry | 64 params, 16 KiB together |
| Tasks of one run | `tasks.task_limit_per_run` (10 by default, 1000 at most) |

## 6. Effects

The worker gives the script the variables `NARSOR_SERVER`, `NARSOR_TOKEN`
(the run token) and `NARSOR_OBSERVER_RUN`. With them, two `narsor-cli`
commands go to the routes of this run. The run token is not valid for other
commands. Only a `check` run may use them: the control plane refuses them
from an `in` or `out` run (HTTP 400, `not_a_check_run`).

### Create a task

```sh
narsor-cli task create --data "$JSON" </dev/null >&2
```

- **A new task on each call.** Narsor does not check whether the object
  already has a task. The script decides that from its state (section 5).
- **The queue decides the task.** The workflow, the repositories, the
  backend and the profile come from `tasks` of the queue file, not from the
  script. The data has only the fields that `tasks.mapping` maps to
  workflow inputs. Narsor refuses a field that is not in the map.
- **Task limit.** A run can create at most `tasks.task_limit_per_run`
  tasks. Narsor refuses the next one, the command exits with `1`, and the
  pair stops in the state `needs_attention` (section 8).
- **Owner and origin.** Each task from a queue belongs to the owner of the
  installation, and it records the queue, the automation and the run.

### Answer a gate

```sh
narsor-cli task decide "$TASK_ID" --episode "$EPISODE" \
  --choice check_feedback --reason "the label was removed" </dev/null >&2
```

or, with the whole answer as JSON (the same object as the answer of a
`script` step, without artifacts):

```sh
narsor-cli task decide "$TASK_ID" --episode "$EPISODE" \
  --data '{"choice": "revise_plan", "reason": "…", "variables": {"note": "…"}}' </dev/null >&2
```

| Option | Meaning |
|---|---|
| `--choice CHOICE` | The name of a choice of the gate. |
| `--reason TEXT` | Why the script chose it. The task history shows it. |
| `--var NAME=VALUE` | A variable of the gate, as text (repeatable). |
| `--var-file NAME=PATH` | A variable of the gate whose value is the text of a file (repeatable). |
| `--data JSON` | The whole answer, `{"choice", "reason", "variables"}`. Not together with the options above. |

- **Only from its entry.** `TASK_ID` and `EPISODE` must be the `task_id`
  and the `episode` of the entry of this run.
- **The same checks as for a person.** Narsor refuses the answer when the
  task does not wait in this episode now, when the choice is not a choice of
  the gate, when a variable that the choice requires has no value, or when
  a variable is not declared by the gate. The command then exits with `1`.
- **The first answer wins.** The first exit of an episode, from a person,
  a script or an operator, wins. A later answer for the same episode is
  refused. A retry of the same answer in the same run gets the input that
  the first call queued.
- **Held for `out`.** When the queue has `out` scripts, the answer waits
  until they ended (section 4).
- **A paused task.** The answer waits in the task, and the task resumes, as
  for an answer of a person.
- **Who answered.** The task history shows the automation and its run, and
  the reason of the script.
- **What the command prints.** `answered STEP of task TASK with CHOICE
  (input ID); the gate exits with it unless another answer came first`.
  The answer is queued, not yet applied by the runner. The script cannot
  know at once that it won. This is safe: the episode protects the gate.
- **No artifacts.** An answer has a choice, a reason and variables only.

## 7. Schedule

The schedule of a queue is one of:

- **an interval**: `interval_secs`, from 60 seconds to 30 days;
- **a cron expression**: `cron`, five fields (minute, hour, day of the
  month, month, day of the week), in UTC. For example, `*/10 * * * *` runs
  at :00, :10, :20 … and `0 8 * * 1-5` runs at 08:00 UTC on each working
  day.

Narsor refuses a cron expression with a seconds field (six fields), an
alias (`@daily`), an expression that never fires, and an expression that
can fire more often than every 60 seconds.

The schedule is for `check` only; `in` and `out` run when a gate episode
enters or leaves the queue.

- **A tick.** The next tick counts from the last tick, not from the end of
  the last run.
- **One run at a time.** A tick that finds the previous run of the pair
  still active, or a round of entries that is not finished, is skipped. It
  is not queued.
- **Rounds of `on_episodes`.** At a tick, each pair takes the entries that
  are on its queue at that time and are ready (every `in` succeeded). It
  runs one time for each entry, one after the other. An entry that is added
  during a round waits for the next round. No entries at a tick: no run, no
  error.
- **Before each run of an entry**, Narsor checks that the task still waits
  in the episode of the entry. When it does not, Narsor does not run the
  script.
- **Restarts.** After the control plane was stopped, a pair runs one time,
  not one time for each tick that it missed.

## 8. States, failures and changes

Each pair (queue, automation) has a state:

| State | Meaning |
|---|---|
| `active` | Runs on the schedule of its queue. |
| `needs_attention` | A run went over the task limit. The pair stopped. |
| `error` | 5 `check` runs in a row failed. The pair stopped. |

**A stopped pair starts again when its definition changes:** the queue
file, the `automation.yaml` or the `check.sh` of the automation. Look at
the errors in the queue view, fix the cause in the catalogue (or the
source), and change the file; the pair is `active` again with a failure
count of 0. A change of `in.sh` or `out.sh` does not restart a pair: those
scripts are not part of the schedule.

- **Failures.** After a failed `check` run of an `always` queue, the next
  run comes after the interval times 2 for each failure in a row (2, 4, 8
  …), at most 1 hour (or the interval, when it is longer). With a cron
  schedule, the run comes at the first fire time after that wait. An error
  of an entry is not counted. An `in` or `out` attempt never changes the
  schedule, the failure count or the state of the pair.
- **Errors.** Narsor keeps a record only of a run that failed or was lost:
  the reason, the exit code, and stdout and stderr (masked). It keeps the
  20 newest records of each pair, and the newest record of each entry. A
  successful run changes only the state and the time of the last run.
- **Lost runs.** When a worker does not report a run before its deadline
  (the time limit plus 1 minute), the run is lost. Its token stops working.
  A lost `check` run keeps the state and is not a failure of the script; a
  lost `in` or `out` run is a failed attempt.
- **No worker.** When a due run waits for a worker for longer than one tick
  of the schedule, the queue views show `no worker`.
- **A queue removed from the catalogue** stops running. Its gates wait for
  a person. The tasks that its scripts created stay.

## 9. Queue views

The queue views are read-only. They show the queues and automations of the
catalogue, the files that cannot be used, and from the state the entries,
the runs and the errors. To change a queue or an automation, change its
file in the catalogue.

- **TUI**: press `O` on the task list. The list shows each queue with its
  number of entries and a mark on a queue that does not run, and under it
  the catalogue files that cannot be used. `Enter` opens a queue: its
  automations (description, scripts, state, last and next run, errors,
  secret names), its entries (task, step, episode, start, params, and for
  each automation how it handled the entry), and the problem of its file.
- **WebUI**: the queues page (`Q`, or the button in the header). It shows
  the same data.
- **CLI**: `narsor-cli queue list` and `narsor-cli queue show QUEUE`
  ([CLI reference](cli.md#queue)).

The task list shows one mark on a task while the current wait episode of
the task has a queue entry. The task view shows what section 4 says: the
`failure` status of a gate step whose `in` failed, and the attempts of `in`
and `out` in *Przebieg*.

## 10. Secrets

An automation names its secrets in `automation.yaml`
(`secrets: [{name, env}]`). The control plane keeps only these names. The
values are files in the secrets directory of the worker (the setting
`server.observer.secrets_dir`), one file for each name, in one flat directory:
`{secrets directory}/{name}` (see
[installation](../operations/installation.md#run-the-observer-worker)).

- The script gets a secret only as the variable `env`, and only the
  secrets that its automation declares. It does not get the environment of
  the worker, and not the worker key. Exception: with
  `server.observer.insecure_same_user: true` the script also gets the server's
  environment (`HOME`, `PATH`, the tools' logins), but never a `NARSOR_*`
  variable; then an automation may use, for example, the login of `glab`
  instead of a secret.
- When a declared secret file is missing, the run fails before its script
  starts. The queue view shows the error.
- The worker refuses to start when the secrets directory is open to other
  users than its owner (`chmod 700`).
- The worker replaces each secret value and the run token in stdout and
  stderr with `[masked]` before it reports them. This is a second line of
  defence: a script must not print a secret.
- Do not put a secret on a command line, because other users can read it.
  The examples give the token to `curl` on stdin (`--config -`).
- **Params are not trusted.** The params of an entry come from a workflow.
  A script must check a URL in the params (its host and its form) before it
  sends a token to it. The example scripts do this, and do not fetch a URL
  that is not theirs. A host check alone still lets an agent name another
  project on the same host, so give the label automations an allowlist:
  `condition.gitlab.projects` (project paths) and `condition.github.repos`
  (`owner/repo`). A URL outside the list is not the automation's entry, and
  the token is not sent. Also give the token access only to those projects.
- **Label names are compared without case**, as GitHub does, so a label
  that differs only in case still counts as present.

## 11. Data from the source

- **The data is not trusted.** Text from Jira, GitHub or GitLab goes into
  task inputs and gate answers (the reason and the variables). A person who
  can write in the source can write instructions for an agent there. In a
  workflow that a queue starts or answers, give the agent steps narrow
  permissions, and treat inputs and answer variables as data, not as
  instructions.
- **Send only what the workflow needs.** Narsor accepts only the mapped
  fields. Do not map fields with personal data (for example, the reporter
  or the assignee of an issue, comments, e-mail addresses). Before you send
  free text such as a summary or a description, make sure what it holds.
- **Check before you use.** Quote every variable. Do not use data from the
  source or from the params as a command, a path or an option without a
  check.

## 12. Test a script

Each example reads the answers of its source from files when
`FAKE_SOURCE_DIR` is set, writes the changes it would send to
`$FAKE_SOURCE_DIR/writes.log`, and does not use the network. With a
stand-in `narsor-cli` on `PATH` that records its arguments, you can run a
script without a source and without Narsor:

```sh
mkdir -p fake/bin
printf '#!/bin/sh\necho "narsor-cli $*" >&2\n' > fake/bin/narsor-cli
chmod +x fake/bin/narsor-cli
echo '{"issues": [{"id": "101", "key": "ABC-101", "fields": {}}]}' > fake/search.json
echo '{"contract_version": 3, "phase": "check",
       "queue": "jira-intake", "automation": "jira-new-issues",
       "condition": {"base_url": "https://jira.example.com", "jql": "project = ABC"},
       "state": {"last_id": 100},
       "first_run": false, "import_backlog": false}' |
  PATH="$PWD/fake/bin:$PATH" FAKE_SOURCE_DIR="$PWD/fake" \
  sh examples/catalog/automations/jira-new-issues/check.sh
```

The header of each review script tells which files of the fake source it
reads. Its input needs an `entry` (section 5), and an `out` input an
`exit`.

Narsor has no dry run: the first run after a file goes into the catalogue
uses the real source and has real effects. Test against the real source by
hand first, with a token that can only read where you can, and set a low
`task_limit_per_run` for the first runs.

## 13. Update from stored observers

Earlier versions kept observers in `observers/` below the old state root
(`global.root` with the built-in values), with
script versions and approvals, and managed them with `narsor-cli observer
…` and the forms of the TUI and the WebUI. That is removed: Narsor no
longer reads those records, and the server logs one warning at start while
the directory has them. Write each observer again as an automation and a
queue in the catalogue (the condition, the schedule, the mode, the secret
names and the workflow mapping move to the queue and automation files, and
the script gets contract version 3). The order of the update is in
[Updates](../operations/updates.md#update-to-catalogue-automations-d78).
