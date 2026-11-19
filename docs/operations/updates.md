# Updates

This page tells you what to do before and after an update that changes the
protocol between the processes or the stored data. Updates that are not
listed here need no special order. The update notes of the remote backends
(automatic resume, the read-only runner) are in
[Remote execution backends](remote-backends.md).

## Update to catalogue automations (D78)

This update moves the configuration of observers from the state to the
catalogue. Automations (`{catalog}/automations/{name}/`) and queues
(`{catalog}/queues/{name}.yaml`) are files, as workflow definitions are.
An automation can have entry and exit scripts (`in.sh`, `out.sh`) that run
when a gate episode enters and leaves its queue. See
[Automations and queues](../user/observers.md). The update is a hard
cutover: Narsor does not migrate the observers stored in
`{state}/observers/`.

### What changes

| Part | Before | After |
|---|---|---|
| Configuration | Observer records in `{state}/observers/`, with script versions and approvals | Catalogue files. No create, approve, pause, resume or delete step: a queue in the catalogue is active. The old records are not read. |
| Runtime state | Per observer | Per pair (queue, automation): `{state}/observer-queues/automations/{queue}/{automation}/`. The queue entries (`{state}/observer-queues/entries/`) keep their shape. |
| Script input | contract version 2 (`observer`) | contract version 3: `phase` (`in`, `check`, `out`), `queue`, `automation`, and `exit` for `out`. |
| Secrets | `{secrets dir}/{owner}/{name}` | `{secrets dir}/{name}`, one flat directory |
| CLI | `narsor-cli observer …` | Removed. `narsor-cli queue list` and `narsor-cli queue show QUEUE`, read-only. |
| HTTP API | Owner routes `/control/v1/observers…` | Removed (`404`). `GET /control/v1/observer-queues` and `GET /control/v1/observer-queues/{queue}` remain, read-only. |
| TUI and web UI | Forms to create, edit, approve, pause, resume and delete observers | Read-only queue views. A waiting gate offers *Uruchom od nowa*. |
| Worker | Asks for a run every 10 seconds | The server holds the claim up to 25 seconds. |

### Before the update

1. **Write the catalogue files.** For each observer that you keep, write an
   automation (`automation.yaml` with its description, params and secret
   names, and `check.sh`) and a queue file (its automations, mode,
   schedule, condition and, for an observer that created tasks, `tasks`
   with the workflow mapping). Change each script to contract version 3.
   Start from [`examples/catalog/`](../../examples/catalog/). The example
   review automations add and remove the review label themselves: remove
   the label steps from your workflows (as
   `workflows/compound-engineering-change.yaml` did). Put the files into
   the catalogue before the update, so that the new server finds them at
   its first start.
2. **Move the secrets.** Move each secret file from the owner's
   subdirectory up into the secrets directory itself
   (`{secrets dir}/{name}`).
3. **Tasks that wait at a watched gate need not finish.** Their queue
   entries keep their shape, and the new `check` scripts serve them. The
   entry and exit scripts do not run for an episode that began before the
   update; to get them, choose *Uruchom od nowa* on the gate after the
   update (this needs the new runner).

### Update order

1. Stop the server (`narsor-server`). It stops the observer worker first.
2. Install the new binaries of the server and the observer worker, and the
   new clients (`narsor`, `narsor-cli`, `narsor-web`). An old client gets
   `404` for the removed observer routes. Install the new runner
   (`narsor-runner`, or the new runner image for the `kubernetes` backend)
   in the same window: an old runner refuses the restart of a waiting gate,
   and the TUI and the web UI show the refusal.
3. Start the server. While `{state}/observers/` still holds observer
   records, it logs one warning at start.
4. Check the catalogue: `narsor-cli queue list` shows each queue, and under
   the queues each catalogue file that cannot be used, with the reason.
   Fix each problem in its file.

### After the update

1. **Look at the first runs.** `narsor-cli queue show QUEUE` shows the
   automations of the queue with their state, runs and errors, and its
   entries.
2. **Delete the old observer records** when the queues work:
   `{state}/observers/`. Narsor does not read them any more, and the start
   warning goes away.

### Rollback

A rollback to the previous server loses the state of the entry and exit
scripts. The previous server does not know the fields
`WatchedGate.automation` and `TaskRecord.exiting_gates`, and it drops them
when it writes the task again. Then:

- an exit that waits for `out` (an answer or an operator command) is no
  longer held: it goes to the runner at the next poll, and `out` does not
  run for it;
- `in` and `out` that did not run yet never run, and the task view no
  longer shows the attempts of the scripts.

Before a rollback, let the tasks leave the gates whose scripts run:

1. Stop new exits: do not answer gates and do not send operator commands.
2. Wait until no task is in the phase `entering` or `exiting`.
   `narsor-cli task show TASK` shows the phase in its `Gate` block, and
   `narsor-cli task show TASK --json` in `gate_automation.phase`.
3. Then stop the server and install the previous binaries.

The two sections below are for updates to earlier versions. The observer
commands that they name are removed by the update above.

## Update that removes the observer source field

This update removes the free-text `source` field of observers. Narsor did
not use it for anything: the script reads the external system with its
`condition` and `secrets`. The update is a hard cutover of the observer
API between the server and its clients.

### What changes

| Part | Before | After |
|---|---|---|
| HTTP API | `POST /control/v1/observers` requires `source`; observer views have `source` | A create request with `source` is refused (`400`, `invalid_request`). Observer views have no `source`. |
| CLI | `observer create NAME --source TEXT …`; `observer show` prints `Source:` | `--source` is an unknown argument (exit code `2`). No `Source:` line. |
| TUI and web UI | The create form has a source field; the detail shows the source | No source field. |
| Script input | `observer` has `id`, `name` and `source` | `observer` has `id` and `name`. The contract version stays `2`. |
| Observer records | `config.source` is stored | Old records load. The next save removes `source`. No migration is necessary. |

### Before the update

Find the approved observer scripts that read `observer.source` (for
example `jq -r .observer.source`). After the update, such a script gets
`null` and no error. Change the script to read the value from its
`condition`, then save it and approve the new version.

### Update order

Update the server and all clients (`narsor`, `narsor-cli`, `narsor-web`)
in the same maintenance window. Clients of different versions do not work
with the server:

- An old CLI or TUI cannot read the observer views of a new server:
  `observer list`, `observer show` and the queues view fail.
- A new client cannot create an observer on an old server: the old server
  requires `source`.

### After the update

Reload each open web UI tab. A tab that still has the old page sends
`source`, and the server refuses each observer that it creates.

## Update to observer queues (D77)

This update removes the `wait` step and the watch lists of observers. A
`decision` gate with a `watch` block replaces them: observers of its queue
answer the gate with `narsor-cli task decide`. See
[Observers](../user/observers.md) and
[the workflow schema](../dev/workflow-schema.md) (§3.3.D). The update is a
hard cutover: there is no transition period and no fallback between the old
and the new versions.

### What changes

| Part | Before | After |
|---|---|---|
| Workflow | `type: wait` step with `event_type`, `filter` and `watch` | Removed. A file with a `wait` step does not load. Use `watch: {queue, params}` on a `decision` step with `by: human`. |
| Templates | `{{ context.event }}` | Removed. |
| Runner protocol | `events` in the poll answer, `POST /runner/v1/events/{id}/ack`, `event_wait` in the snapshot | Removed. The snapshot has `gate_watch`. |
| Observer records | watch mode, watch list, `deliver_event` effect | Read as version 2: the queue is the observer's name, `watch_list` becomes `on_episodes`, `all` becomes `always`. The watch entries are dropped. A `deliver_event` observer is paused. |
| Script input | contract version 1 (`watch`, exit code `1` = check again) | Contract version 2 (`entry`; only exit code `0` is a success). |
| CLI | `task event`, `observer watch …`, `--deliver-events`, `--watch-list`, `--allow-prefix`, `--max-object-len` | Removed. New: `task decide`, `observer update`, `observer queue list\|show`, `observer script show`, `--queue`, `--mode`, `--cron`. |

### Before the update

1. **Find the tasks of workflows with a `wait` step.** Each execution keeps
   its own copy of the workflow definition (D48). After the update, a
   definition with a `wait` step does not load, so such a task cannot run
   or resume. This is true for each task of such a workflow, also when the
   task is not in the `wait` step now. Let these tasks finish, or archive
   them (`narsor-cli task archive TASK`). Then start them again from the
   changed workflow after the update.
2. **Change your workflows.** Replace each `wait` step with a `watch` block
   on the `decision` gate that comes after it, and remove each use of
   `{{ context.event }}`. Check each file with `narsor-cli task validate
   PATH` after the update. The bundled workflows are already changed.
3. **Prepare the scripts of event observers.** A script that delivered
   events (`narsor-cli task event`) must be changed to read contract
   version 2 and to answer with `narsor-cli task decide`. The example
   scripts of that version (`workflows/observers/examples/`, now replaced
   by `examples/catalog/`) showed how.
4. **Pause the running tasks** (`narsor-cli task pause TASK`). A runner of
   the old version must not run against the new control plane.

### Update order

Update all parts in the same maintenance window:

1. Stop the server (`narsor-server`). It stops the observer worker first.
2. Install the new binaries of the server, the controller
   (`narsor-controller`), the runner (`narsor-runner`, next to the
   controller for the `local` backend) and the observer worker. For the
   `kubernetes` backend, use the new runner image.
3. Install the new clients (`narsor`, `narsor-cli`, `narsor-web`). A client
   of the old version shows the new history warning `gate_queue_warning` as
   an unknown event.
4. Start the server.
5. Resume the paused tasks (`narsor-cli task resume TASK`). A new runner
   takes each of them.

### After the update

1. **Look at the observers.** `narsor-cli observer list` shows the state
   and the note of each observer, and `narsor-cli observer show OBSERVER`
   its queue and mode. A former `deliver_event` observer is `paused`, and
   its note says why. For each one:
   - set the queue of the gates that it must answer:
     `narsor-cli observer update OBSERVER --queue QUEUE --mode on_episodes`;
   - save the changed script: `narsor-cli observer script set OBSERVER
     --file FILE`;
   - read it and approve it: `narsor-cli observer script show OBSERVER`,
     then `narsor-cli observer approve OBSERVER --version N`;
   - resume it: `narsor-cli observer resume OBSERVER`.
2. **Old watch lists are gone.** The CLI refuses the removed flags and
   commands and names what replaces them. You do not add entries by hand
   any more: a gate with a `watch` block adds them.
3. **Check the queues.** `narsor-cli observer queue list` shows each queue
   with its mode and its number of entries. A gate whose queue has no active
   `on_episodes` observer writes `gate_queue_warning` to the task history
   and waits for a person.
4. **Script errors after the update.** A script of contract version 1 that
   was not changed fails on its first run (the example scripts stop when
   `contract_version` is not `2`). `narsor-cli observer show OBSERVER` shows
   the error.
