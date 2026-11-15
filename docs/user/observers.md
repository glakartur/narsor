# Observers

An observer looks at an external source (Jira, GitHub, GitLab, or any other
source that a shell script can read) on a schedule, and does one of two
things in Narsor:

- **create a task** for each new object that matches its condition (for
  example, a new Jira issue); or
- **deliver an event** to a task that waits for it (for example, the
  approval of a merge request that a `wait` step waits for).

Narsor has no built-in client for Jira, GitHub or GitLab. A shell script that
you write and approve talks to the source. Narsor keeps the schedule,
limits the effects of each run and records where each task came from. The
script remembers in its state what it already reported (section 3).

This page tells you how to write an observer and its script. The commands
are in [the CLI reference](cli.md#observer). How the server starts the worker is
in [installation](../operations/installation.md#run-the-observer-worker). The
`wait` step and its `watch` block are in
[the workflow schema](../dev/workflow-schema.md) (§3.3.D, WaitAction).

> **Run only scripts that you trust.** In this release the worker runs a
> script as a separate system user on the worker's host, with the secrets of
> its observer. Read every script before you approve it, also a script that
> an agent wrote.

## 1. How an observer works

```text
control plane                      worker                       script
-------------                      ------                       ------
the run is due  ───────────────▶  claims the run
                                   (worker key)
                                   starts `sh` with the input
                                   on stdin and the run token  ─▶ reads the source
                                                                  narsor-cli task create …
              ◀────────────────────────────────────────────────── narsor-cli task event …
                                   reads stdout and stderr     ◀─ prints {"state": …}
saves the state ◀───────────────  reports the result
or counts a failure                (secrets masked)
```

1. The control plane keeps the schedule. When the next run of an active
   observer is due, the run waits for a worker. Each observer has at most
   one run at a time.
2. The worker (`narsor-cli observer worker`, which the server starts) claims
   the run. It gets a token that is valid only for this run and only until
   the run's deadline.
3. The worker starts the approved script version with a clean environment.
   The script reads its input on stdin.
4. The script reads the source and makes its effects with `narsor-cli task
   create` and `narsor-cli task event`. These commands use the run token.
5. The script prints its new state on stdout. The worker reports the exit
   code, stdout and stderr. A successful run saves the new state. Any other
   result keeps the old state.

## 2. Make an observer

The owner of the installation does all the steps through the CLI. A task
cannot save or approve a script.

1. **Write the script.** Start from an example in
   [`workflows/observers/examples/`](../../workflows/observers/examples/):

   | Example | Source | Effect |
   |---|---|---|
   | `jira-new-issues.sh` | Jira search (JQL) | A task for each new issue |
   | `github-pr-review.sh` | GitHub pull requests on the watch list | `review.approved`, `review.changes_requested`, `merged`, `closed` |
   | `gitlab-mr-review.sh` | GitLab merge requests on the watch list | The same event types |

   The header of each example gives the `observer create` command, the
   condition, the secrets and how to test it with a fake source.

   An agent can help you write the script outside Narsor: give it this
   page and the examples. The script is not part of a workflow.

2. **Create the observer.** It starts in the state `draft`, and nothing
   runs:

   ```sh
   narsor-cli observer create jira-new --source jira \
     --workflow triage --map ticket=key \
     --secret jira-token=JIRA_TOKEN --task-limit 10 \
     --condition '{"base_url": "https://jira.example.com", "jql": "project = ABC", "fields": ["key"]}'
   ```

3. **Save the script.** Each save makes a new version that does not
   change. A save never approves a version:

   ```sh
   narsor-cli observer script set jira-new --file jira-new-issues.sh
   ```

4. **Test it** with a fake source before you approve it (section 9).
   Narsor has no dry run: the first run of an approved version uses the
   real source and has real effects.

5. **Approve it.** The schedule runs only the approved version:

   ```sh
   narsor-cli observer approve jira-new --version 1
   ```

When you save version 2 of an active observer, the schedule continues to
run version 1 until you approve version 2.

## 3. The script contract

The script is a POSIX `sh` script. It can use `curl`, `jq`, `narsor-cli` and
the tools of a base system.

### Input (stdin)

One JSON document (contract version 1):

```json
{
  "contract_version": 1,
  "observer": {"id": "obs-…", "name": "pr-review", "source": "github.com"},
  "condition": {"host": "github.com"},
  "state": {"last_id": 10342},
  "watches": [
    {
      "object": "https://github.com/acme/app/pull/17",
      "task_id": "…",
      "event_types": ["review.approved", "merged"],
      "wait_episode": "…"
    }
  ],
  "first_run": false,
  "import_backlog": false
}
```

| Field | Meaning |
|---|---|
| `contract_version` | `1`. Stop with an error when it has a different value. |
| `observer` | The id, name and source of the observer. |
| `condition` | The JSON object of `--condition`, as the owner gave it. |
| `state` | What the last successful run printed in `state`. `null` before the first successful run. |
| `watches` | The watch entries (only for an observer with a watch list). `wait_episode` is the id of the `wait` episode of the task, or `null` when the task does not wait now. |
| `first_run` | `true` until the first run succeeds. |
| `import_backlog` | `true` when the owner gave `--import-backlog`. |

### Output (stdout)

Exactly one JSON document, and nothing else:

```json
{"state": {"last_id": 10350}, "closed_watches": ["https://github.com/acme/app/pull/17"]}
```

- `state` is required. The next run gets it in its input. When the script
  has nothing to keep, it prints `{"state": {}}`.
- `closed_watches` is optional. It lists the watched objects that ended
  (for example, a merged or closed pull request). Narsor removes their
  watch entries. The list has at most 10000 entries. Each entry has 1 to
  4096 bytes and no control characters. Otherwise the output is not valid.
- Other fields, a second document or a log line after the document make
  the output not valid.

Write all other text to stderr. This includes the output of `narsor-cli`:
send it to stderr with `>&2`.

### Result

| The script | Narsor |
|---|---|
| exits with `0` and prints a valid document | Saves the new state, removes the closed watch entries and sets the failure count to 0. |
| exits with a different code, prints a document that is not valid, or runs for longer than its time limit | Keeps the old state. The next run sees the same objects again. Counts one failure. |

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

### Limits

| Limit | Value |
|---|---|
| Time of one run | 5 minutes. Then the worker stops the script and all its processes. |
| stdout | 1 MiB |
| stderr | 64 KiB (Narsor keeps the start) |
| Data of one task (`--data`) | 64 KiB |
| Data of one event (`--data`) | 16 KiB |
| Entries in `closed_watches` | 10000, each 1 to 4096 bytes |
| Tasks of one run | `--task-limit` (10 by default, 1000 at most) |
| Time between runs | `--interval` (300 s by default, 60 s at least) |

## 4. Effects

The worker gives the script the variables `NARSOR_SERVER`, `NARSOR_TOKEN`
(the run token) and `NARSOR_OBSERVER_RUN`. With them, two `narsor-cli`
commands go to the routes of this run. The run token is not valid for other
commands.

### Create a task

```sh
narsor-cli task create --data "$JSON" </dev/null >&2
```

- **A new task on each call.** Narsor does not check whether the object
  already has a task. The script decides that from its state (section 3).
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

### Deliver an event

```sh
narsor-cli task event "$TASK_ID" --type review.approved --data "$JSON" </dev/null >&2
```

- The task must be on the watch list of this observer.
- Narsor writes the event to the history of the task.
- For Narsor, an event that the task waits for is the same as the owner's
  choice of a way out of a step. The event ends a `wait` step only when the
  task waits now, the type is the `event_type` of the step and the data
  passes its filter. Narsor keeps the event in the task and resumes the
  task. When the runner of the task does not run at that time, the event
  waits on the server, and the runner takes it when it runs again. An event
  that comes before the task waits stays only in the history.
- **Send an event only when the task waits.** When `wait_episode` is
  `null`, the task does not wait: do not send the event.
- **Close the entry after the event.** The example scripts report the
  object in `closed_watches` after they send a `review.*` event, so that a
  task gets one event for each wait. When the task waits again (a review
  loop), its `wait` step adds the object to the watch list again.

## 5. Watch entries (pull and merge requests)

An observer created with `--deliver-events` (or `--watch-list`) looks only
at the objects on its watch list. When the list is empty, the observer does
not run.

An entry is added in one of two ways:

- **From a workflow.** A `wait` step with a `watch` block adds an entry when
  the step starts to wait. See
  [the workflow schema](../dev/workflow-schema.md) (§3.3.D).
- **By the owner.** `narsor-cli observer watch add OBSERVER --task TASK_ID
  --object URL --events TYPE[,TYPE…]`.

An object must start with one of the observer's `--allow-prefix` values and
must not be longer than `--max-object-len`. A URL prefix includes the host,
so that a task cannot send the token of the source to a different host. The
example scripts also check the host before they use the token.

An entry goes away when the script reports the object in `closed_watches`,
when the task is finished or archived, or when the owner removes it
(`observer watch remove`).

> **A pull request that comes back to the list.** When a task waits again
> for the same pull or merge request, the old review in the source can
> still match, and the script would send the same event at once. To prevent
> this, mark the pull request before it goes back to the watch list (for
> example, with a label that the script looks for). This is not part of the
> example scripts: do it in your workflow or in your script.

## 6. Schedule, failures and states

| State | Meaning |
|---|---|
| `draft` | No version is approved. Nothing runs. |
| `active` | Runs on its schedule. |
| `paused` | The owner stopped it (`observer pause`). |
| `needs_attention` | A run went over its task limit. It waits for the owner. |
| `error` | 5 runs in a row failed. It waits for the owner. |

`observer resume` puts a `paused`, `needs_attention` or `error` observer
back on its schedule and sets the failure count to 0. `observer approve`
makes a `draft` observer `active`. It does not change other states.

- **Failures.** After a failed run, the next run comes after the interval
  times 2 for each failure in a row (2, 4, 8 …), at most 1 hour (or the
  interval, when it is longer).
- **Errors.** Narsor keeps a record only of a run that failed or was lost:
  the reason, the exit code, and stdout and stderr (masked). It keeps the
  20 newest records of each observer. `observer show` shows them. A
  successful run changes only the state and the time of the last run.
- **Lost runs.** When a worker does not report a run before its deadline
  (the time limit plus 1 minute), the run is lost. Its token stops working.
  The state does not change, and a lost run is not a failure of the script.
  `observer show` shows it as an error.
- **No worker.** When a due run waits for a worker for longer than one
  interval, `observer list` and `observer show` show `no worker`.
- **Restarts.** After the control plane was stopped, an observer runs one
  time, not one time for each interval that it missed.

## 7. Secrets

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

## 8. Data from the source

- **The data is not trusted.** Text from Jira, GitHub or GitLab goes into
  task inputs and events. A person who can write in the source can write
  instructions for an agent there. In a workflow that an observer starts,
  give the agent steps narrow permissions, and treat inputs and
  `context.event` as data, not as instructions.
- **Send only what the workflow needs.** Narsor accepts only the mapped
  fields. Do not map fields with personal data (for example, the reporter
  or the assignee of an issue, comments, e-mail addresses). Before you send
  free text such as a summary or a description, make sure what it holds.
- **Check before you use.** Quote every variable. Do not use data from the
  source as a command, a path or an option without a check.

## 9. Test a script

Each example reads the answers of its source from files when
`FAKE_SOURCE_DIR` is set, and does not use the network. With a stand-in
`narsor-cli` on `PATH` that records its arguments, you can run a script
without a source and without Narsor:

```sh
mkdir -p fake/bin
printf '#!/bin/sh\necho "narsor-cli $*" >&2\n' > fake/bin/narsor-cli
chmod +x fake/bin/narsor-cli
echo '{"issues": [{"id": "101", "key": "ABC-101", "fields": {}}]}' > fake/search.json
echo '{"contract_version": 1, "observer": {"id": "o", "name": "jira-new", "source": "jira"},
       "condition": {"base_url": "https://jira.example.com", "jql": "project = ABC"},
       "state": {"last_id": 100}, "watches": [],
       "first_run": false, "import_backlog": false}' |
  PATH="$PWD/fake/bin:$PATH" FAKE_SOURCE_DIR="$PWD/fake" \
  sh workflows/observers/examples/jira-new-issues.sh
```

Narsor has no dry run. Test against the real source by hand, with a
token that can only read, before you approve the script. Set a low
`--task-limit` for the first runs.
