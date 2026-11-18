# Observers

An observer looks at an external source (Jira, GitHub, GitLab, or any other
source that a shell script can read) on a schedule, and does one of two
things in Narsor:

- **create a task** for each new object that matches its condition (for
  example, a new Jira issue); or
- **answer a decision gate** of a task that waits on its queue (for
  example, choose `check_feedback` when the reviewer of a merge request is
  done).

Narsor has no built-in client for Jira, GitHub or GitLab. A shell script that
you write and approve talks to the source. Narsor keeps the schedule,
limits the effects of each run and records where each task and each answer
came from. A script that creates tasks remembers in its state what it
already reported (section 4).

This page tells you how to write an observer and its script. The commands
are in [the CLI reference](cli.md#observer). How the server starts the worker is
in [installation](../operations/installation.md#run-the-observer-worker). The
`watch` block of a `decision` gate is in
[the workflow schema](../dev/workflow-schema.md) (§3.3.D).

> **Run only scripts that you trust.** In this release the worker runs a
> script as a separate system user on the worker's host, with the secrets of
> its observer. Read every script before you approve it, also a script that
> an agent wrote.

## 1. How an observer works

```text
control plane                      worker                       script
-------------                      ------                       ------
a tick of the schedule ────────▶  claims the run
                                   (worker key)
                                   starts `sh` with the input
                                   on stdin and the run token  ─▶ reads the source
                                                                  narsor-cli task create …
              ◀────────────────────────────────────────────────── narsor-cli task decide …
                                   reads the exit code,        ◀─ prints {"state": …}
                                   stdout and stderr              (only to create tasks)
saves the state ◀───────────────  reports the result
or records an error                (secrets masked)
```

1. The control plane keeps the schedule. When a tick of an active observer
   comes, the run waits for a worker. Each observer has at most one run at
   a time. An observer in mode `on_episodes` runs its script one time for
   each entry of its queue: one run checks one entry (section 6).
2. The worker (`narsor-observer-worker`, which the server starts) claims
   the run. It gets a token that is valid only for this run and only until
   the run's deadline.
3. The worker starts the approved script version with a clean environment.
   The script reads its input on stdin.
4. The script reads the source and makes its effects with `narsor-cli task
   create` and `narsor-cli task decide`. These commands use the run token.
5. The worker reports the exit code, stdout and stderr. For an observer
   that creates tasks, the script prints its new state on stdout, and a
   successful run saves it. For an observer that checks queue entries, the
   exit code tells whether the run had an error (section 4).

## 2. Queues and run modes

Each observer serves one **queue**. A queue is a name in your namespace,
not an object of its own: it is the set of your observers that name it,
and the entries that wait on it. Several observers can serve one queue (for
example, one for GitLab and one for GitHub). An observer of another owner
never sees your entries.

Each observer has a **run mode**:

| Mode | When the script runs | Input |
|---|---|---|
| `on_episodes` | Only when its queue has entries: one run for each entry. No entry, no run. | One entry in `entry` |
| `always` | On each tick of its schedule, also when no task waits. | No `entry` |

All observers of one queue have the same mode. Narsor refuses a create or
an update that mixes the modes of a queue, and names the mode of the queue.
Only an `on_episodes` observer can answer a gate. Use `always` for a source
observer that creates tasks (for example, from Jira).

An entry comes from a workflow: a `decision` step with `by: human` and a
`watch` block adds one entry for each wait episode of the gate. See
[the workflow schema](../dev/workflow-schema.md) (§3.3.D) and section 5
(Answer a gate).

## 3. Make an observer

The owner of the installation does all the steps through the CLI, the TUI
or the WebUI (section 8). A task cannot save or approve a script.

1. **Write the script.** Start from an example in
   [`workflows/observers/examples/`](../../workflows/observers/examples/):

   | Example | Source | Mode | Effect |
   |---|---|---|---|
   | `jira-new-issues.sh` | Jira search (JQL) | `always` | A task for each new issue |
   | `github-pr-review.sh` | GitHub pull requests of the queue entries | `on_episodes` | Answers the gate when the review label is removed |
   | `gitlab-mr-review.sh` | GitLab merge requests of the queue entries | `on_episodes` | The same |

   The header of each example gives the `observer create` command, the
   condition, the gate params, the secrets and how to test it with a fake
   source.

   An agent can help you write the script outside Narsor: give it this
   page and the examples. The script is not part of a workflow.

2. **Create the observer.** It starts in the state `draft`, and nothing
   runs:

   ```sh
   narsor-cli observer create jira-new --source jira \
     --queue jira-intake --mode always --cron '*/10 * * * *' \
     --workflow triage --map ticket=key \
     --secret jira-token=JIRA_TOKEN --task-limit 10 \
     --condition '{"base_url": "https://jira.example.com", "jql": "project = ABC", "fields": ["key"]}'
   ```

   ```sh
   narsor-cli observer create gitlab-mr --source gitlab.example.com \
     --queue mr-review --mode on_episodes --cron '*/5 * * * *' \
     --secret gitlab-token=GITLAB_TOKEN \
     --condition '{"host": "gitlab.example.com"}'
   ```

   Without `--queue`, the queue is the name of the observer. Without
   `--mode`, the mode is `always` with `--workflow` and `on_episodes`
   without it.

3. **Save the script.** Each save makes a new version that does not
   change. A save never approves a version:

   ```sh
   narsor-cli observer script set jira-new --file jira-new-issues.sh
   ```

4. **Test it** with a fake source before you approve it (section 12).
   Narsor has no dry run: the first run of an approved version uses the
   real source and has real effects.

5. **Read and approve it.** The schedule runs only the approved version:

   ```sh
   narsor-cli observer script show jira-new
   narsor-cli observer approve jira-new --version 1
   ```

When you save version 2 of an active observer, the schedule continues to
run version 1 until you approve version 2. The views mark the observer
until then (`version 2 not approved`).

To change the queue, the mode or the schedule later, use `observer update`
(section 7).

## 4. The script contract

The script is a POSIX `sh` script. It can use `curl`, `jq`, `narsor-cli` and
the tools of a base system.

There are two kinds of scripts:

- A script that **creates tasks** (mode `always`, for example
  `jira-new-issues.sh`) reads the source one time for each run. It prints
  its state on stdout, and the next run gets it back.
- A script that **answers gates** (mode `on_episodes`, for example
  `github-pr-review.sh`) checks one queue entry in each run. It prints
  nothing on stdout. When it finds an answer, it calls `narsor-cli task
  decide`.

### Input (stdin)

One JSON document (contract version 2):

```json
{
  "contract_version": 2,
  "observer": {"id": "obs-…", "name": "github-pr", "source": "github.com"},
  "condition": {"host": "github.com"},
  "state": null,
  "entry": {
    "task_id": "…",
    "episode": "…",
    "episode_started_at": "2026-11-17T10:00:00Z",
    "step": "review_work",
    "params": {"mr_url": "https://github.com/acme/app/pull/17", "label": "narsor-review"},
    "choices": [
      {"name": "check_feedback", "description": "The review is done.", "requires_variables": []},
      {"name": "revise_plan", "description": null, "requires_variables": ["note"]}
    ],
    "variables": [
      {"name": "note", "type": "text", "description": "What to change in the plan."}
    ]
  },
  "first_run": false,
  "import_backlog": false
}
```

| Field | Meaning |
|---|---|
| `contract_version` | `2`. Stop with an error when it has a different value. |
| `observer` | The id, name and source of the observer. |
| `condition` | The JSON object of `--condition`, as the owner gave it. |
| `state` | What the last successful run printed in `state`. `null` before the first successful run, and always `null` for an `on_episodes` observer. |
| `entry` | The one queue entry that this run checks. Only for an `on_episodes` observer; an `always` observer gets no `entry` field. |
| `entry.task_id`, `entry.episode` | The task that waits and its wait episode. An answer names both. |
| `entry.episode_started_at` | When the control plane added the entry. Whether and how the script uses it is up to the script. The example scripts answer only when the merge request changed after this time. |
| `entry.step` | The workflow state of the gate. |
| `entry.params` | The rendered `watch.params` of the gate, as text. Narsor does not check them: the script must check each value before it uses it. |
| `entry.choices` | The choices of the gate, in the workflow's order: name, description, and the variables that an answer with this choice must give. |
| `entry.variables` | The variables that an answer may give: name, type, description and, when the workflow restricts them, `options`. |
| `first_run` | `true` until the first run succeeds. |
| `import_backlog` | `true` when the owner gave `--import-backlog`. |

Version 1 had a `watch` object and the exit code `1` ("check again"). Both
are removed: a script of version 1 must be changed before it runs again
(section 10).

### Result of a script that creates tasks

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
tasks. The script must remember in its state what it already reported, so
that it does not create a task on each run.

> **Risk of a duplicate task.** When a script creates a task and its new
> state is then not saved (the script fails later in the same run, or the
> worker or the control plane stops before the result), the next run starts
> from the old state and can create the same task again. Keep the window
> small: when a task is not created, stop creating tasks and still print
> the state up to the last task you created, as `jira-new-issues.sh` does.
> When the run created no task at all, exit with an error instead: the
> error is then recorded and counted, and a refusal that does not go away
> brings the observer to `error` after 5 failed runs in a row.

### Result of a script that answers gates

The exit code is the result. Narsor does not read stdout.

| Exit code | Meaning | Narsor |
|---|---|---|
| `0` | The run is done: the script answered the gate, or it has nothing to answer in this run. | Records the time of the run for the entry. |
| any other code, or the script runs for longer than its time limit | An error. | Records the error for the entry. A new error of the same entry replaces the old one. |

No exit code removes an entry. The entry stays on its queue until its gate
exits, and the next round checks it again. An error of an entry is not
counted as a failure: the observer does not wait longer and does not go to
`error`.

When several observers serve one queue, each script must recognize its own
entries. A script that does not recognize an entry (for example, the URL is
on another host) exits with `0` and gives no answer. It must not use its
token for that entry.

### Limits

| Limit | Value |
|---|---|
| Time of one run | 5 minutes. Then the worker stops the script and all its processes. |
| stdout | 1 MiB |
| stderr | 64 KiB (Narsor keeps the start) |
| Data of one task (`--data`) | 64 KiB |
| Params of one entry | 64 params, 16 KiB together |
| Tasks of one run | `--task-limit` (10 by default, 1000 at most) |
| Schedule | `--interval` (300 s by default, 60 s at least, 30 days at most) or `--cron` (section 6) |

## 5. Effects

The worker gives the script the variables `NARSOR_SERVER`, `NARSOR_TOKEN`
(the run token) and `NARSOR_OBSERVER_RUN`. With them, two `narsor-cli`
commands go to the routes of this run. The run token is not valid for other
commands.

### Create a task

```sh
narsor-cli task create --data "$JSON" </dev/null >&2
```

- **A new task on each call.** Narsor does not check whether the object
  already has a task. The script decides that from its state (section 4).
- **The observer decides the task.** The workflow, the repositories, the
  backend and the profile come from the observer, not from the script. The
  data has only the fields that the observer maps to workflow inputs
  (`--map INPUT=FIELD`). Narsor refuses a field that is not in the map.
- **Task limit.** A run can create at most `--task-limit` tasks. Narsor
  refuses the next one, the command exits with `1`, and the observer goes
  to the state `needs_attention`. Look at the condition, then use
  `observer resume`.
- **Origin.** Each task from an observer has the observer's owner, and it
  records the observer and the run.

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

- **Only from a run.** The run must be the run of an `on_episodes`
  observer, and `TASK_ID` and `EPISODE` must be the `task_id` and the
  `episode` of the entry of this run.
- **The same checks as for a person.** Narsor refuses the answer when the
  task does not wait in this episode now, when the choice is not a choice of
  the gate, when a variable that the choice requires has no value, or when
  a variable is not declared by the gate. The command then exits with `1`.
- **The first answer wins.** The first answer of an episode, from a person
  or from a script, exits the gate. A later answer for the same episode is
  refused. A retry of the same answer in the same run gets the input that
  the first call queued.
- **A paused task.** The answer waits in the task, and the task resumes, as
  for an answer of a person.
- **Who answered.** The task history shows the observer and its run, and
  the reason of the script.
- **What the command prints.** `answered STEP of task TASK with CHOICE
  (input ID); the gate exits with it unless another answer came first`.
  The answer is queued, not yet applied by the runner. The script cannot
  know at once that it won. This is safe: the episode protects the gate.
- **No artifacts.** An answer has a choice, a reason and variables only.

## 6. Schedule

The schedule is a property of the observer. It is one of:

- **an interval**: `--interval SECONDS`, from 60 seconds to 30 days. When
  you give no schedule, the interval is 300 seconds;
- **a cron expression**: `--cron 'EXPRESSION'`, five fields (minute, hour,
  day of the month, month, day of the week), in UTC. For example,
  `*/10 * * * *` runs at :00, :10, :20 … and `0 8 * * 1-5` runs at 08:00
  UTC on each working day.

Exactly one of the two is set. Narsor refuses a cron expression with a
seconds field (six fields), an alias (`@daily`), an expression that never
fires, and an expression that can fire more often than every 60 seconds.

- **A tick.** The next tick counts from the last tick, not from the end of
  the last run.
- **One run at a time.** A tick that finds the previous run still active,
  or a round of entries that is not finished, is skipped. It is not queued.
- **Rounds of `on_episodes`.** At a tick, the observer takes the entries
  that are on its queue at that time. It runs one time for each entry, one
  after the other. An entry that is added during a round waits for the next
  round. No entries at a tick: no run, no error.
- **Before each run of an entry**, Narsor checks that the task still waits
  in the episode of the entry. When it does not, Narsor removes the entry
  and does not run the script.
- **Restarts.** After the control plane was stopped, an observer runs one
  time, not one time for each tick that it missed.

## 7. States, failures and changes

| State | Meaning |
|---|---|
| `draft` | No version is approved. Nothing runs. |
| `active` | Runs on its schedule. |
| `paused` | The owner stopped it (`observer pause`), or the update to queues paused it (section 10). |
| `needs_attention` | A run went over its task limit. It waits for the owner. |
| `error` | 5 runs in a row failed. It waits for the owner. |

`observer resume` puts a `paused`, `needs_attention` or `error` observer
back on its schedule and sets the failure count to 0. `observer approve`
makes a `draft` observer `active`. It does not change other states.

`observer update OBSERVER [--queue QUEUE] [--mode MODE] [--interval
SECONDS | --cron EXPRESSION]` changes the queue, the mode or the schedule.
What you do not give stays. The mode must be the mode of the other
observers of the queue that the observer then serves.

- **Failures.** After a failed run of an observer that creates tasks, the
  next run comes after the interval times 2 for each failure in a row
  (2, 4, 8 …), at most 1 hour (or the interval, when it is longer). With a
  cron schedule, the run comes at the first fire time after that wait. An
  error of an entry is not counted.
- **Errors.** Narsor keeps a record only of a run that failed or was lost:
  the reason, the exit code, and stdout and stderr (masked). It keeps the
  20 newest records of each observer, and the newest record of each entry.
  `observer show` shows them, with the entry of an entry run. A successful
  run changes only the state and the time of the last run.
- **Lost runs.** When a worker does not report a run before its deadline
  (the time limit plus 1 minute), the run is lost. Its token stops working.
  The state does not change, and a lost run is not a failure of the script.
  `observer show` shows it as an error.
- **No worker.** When a due run waits for a worker for longer than one
  interval, `observer list` and `observer show` show `no worker`.
- **Delete.** `observer delete` removes the observer with its scripts and
  run errors. It cannot be undone. The tasks that it created stay. When it
  was the last observer of a queue, the gates of the queue wait for a
  person.

## 8. Queue views

The owner manages queues and observers in three places. All of them use
the same owner routes, and an owner sees only their own observers.

- **TUI**: press `O` on the task list. The list shows each queue with its
  number of entries, and the description of a queue gives its mode. `Enter` opens a queue: its observers
  (name, state, approved and newest script version, schedule, last run) and
  its entries (task, step, episode, start, params, and for each observer
  the last run and the last error of the entry). In a queue, `a` creates an
  observer, `e` edits its queue, mode and schedule, `s` edits the script in
  `$EDITOR` (or a paste field when no editor is set), `v` shows the newest
  version and then approves it, `p` pauses, `r` resumes and `d` deletes
  (with a confirmation that names the observer).
- **WebUI**: the queues page (`Q`, or the button in the header). It shows
  the same data. Its forms create and edit observers, its script editor
  saves a new version, and its approve dialog shows the script text before
  you confirm.
- **CLI**: `narsor-cli observer queue list` and `narsor-cli observer queue
  show QUEUE`.

A save in a view never approves a version. The views show the approved
version and the newest saved version, and mark the observer when they are
not the same.

The views do not show or change the secrets, the task limit, the workflow
mapping and the other settings of `observer create`. They show them
read-only. Set them with the CLI. So a source observer that you create in a
view needs the CLI for its workflow mapping.

The task list shows one mark on a task while the current wait episode of
the task has a queue entry. The task view does not show the state of the
observer runs.

## 9. Secrets

An observer names its secrets with `--secret NAME=ENV`. The control plane
keeps only these names. The values are files in the secrets directory of
the worker. One worker serves the observers of all owners, so the directory
has one subdirectory for each owner, with one file for each name:
`{secrets directory}/{owner}/{NAME}` (see
[installation](../operations/installation.md#run-the-observer-worker)).

- The script gets a secret only as the variable `ENV`, and only the
  secrets of its own observer. It does not get the environment of the
  worker, and not the worker key.
- The worker reads a secret only from the subdirectory of the observer's
  owner. You cannot use the secret of another owner, also when you know its
  name.
- When the owner's name cannot be a directory name, or a secret file is
  missing, the run fails before its script starts. `observer show` shows
  the error.
- The worker replaces each secret value and the run token in stdout and
  stderr with `[masked]` before it reports them. This is a second line of
  defence: a script must not print a secret.
- Do not put a secret on a command line, because other users can read it.
  The examples give the token to `curl` on stdin (`--config -`).
- **Params are not trusted.** The params of an entry come from a workflow.
  A script must check a URL in the params (its host and its form) before it
  sends a token to it. The example scripts do this, and do not fetch a URL
  that is not theirs.

## 10. Update from watch lists

Observers of an earlier version had watch lists and could deliver events
to a `wait` step. The update to queues reads each old observer as follows:

| Before | After |
|---|---|
| no queue | the queue is the name of the observer |
| watch mode `watch_list` | mode `on_episodes` |
| watch mode `all` | mode `always` |
| watch entries, `--allow-prefix`, `--max-object-len` | removed |
| `--deliver-events` (effect `deliver_event`) | the observer is `paused` (a `draft` stays a `draft`), with a note that says why. Nothing is deleted. |
| `--interval` | unchanged |

For an observer that delivered events: change its script to contract
version 2 and `narsor-cli task decide`, set its queue to the queue of the
gate with `observer update --queue`, save and approve the new script, then
resume it. The CLI refuses the removed flags (`--deliver-events`,
`--watch-list`, `--allow-prefix`, `--max-object-len`) and names what
replaces them. The commands `observer watch …` and `task event` are
removed. The order of the update is in
[Updates](../operations/updates.md).

## 11. Data from the source

- **The data is not trusted.** Text from Jira, GitHub or GitLab goes into
  task inputs and gate answers (the reason and the variables). A person who
  can write in the source can write instructions for an agent there. In a
  workflow that an observer starts or answers, give the agent steps narrow
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
`FAKE_SOURCE_DIR` is set, and does not use the network. With a stand-in
`narsor-cli` on `PATH` that records its arguments, you can run a script
without a source and without Narsor:

```sh
mkdir -p fake/bin
printf '#!/bin/sh\necho "narsor-cli $*" >&2\n' > fake/bin/narsor-cli
chmod +x fake/bin/narsor-cli
echo '{"issues": [{"id": "101", "key": "ABC-101", "fields": {}}]}' > fake/search.json
echo '{"contract_version": 2, "observer": {"id": "o", "name": "jira-new", "source": "jira"},
       "condition": {"base_url": "https://jira.example.com", "jql": "project = ABC"},
       "state": {"last_id": 100},
       "first_run": false, "import_backlog": false}' |
  PATH="$PWD/fake/bin:$PATH" FAKE_SOURCE_DIR="$PWD/fake" \
  sh workflows/observers/examples/jira-new-issues.sh
```

The header of each review example tells which files of the fake source it
reads. Its input needs an `entry` (section 4).

Narsor has no dry run. Test against the real source by hand, with a
token that can only read, before you approve the script. Set a low
`--task-limit` for the first runs.
