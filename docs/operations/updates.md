# Updates

This page tells you what to do before and after an update that changes the
protocol between the processes or the stored data. Updates that are not
listed here need no special order. The update notes of the remote backends
(automatic resume, the read-only runner) are in
[Remote execution backends](remote-backends.md).

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
   scripts in `workflows/observers/examples/` show how.
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
