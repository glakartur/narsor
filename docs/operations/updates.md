# Updates

This page tells you what to do before and after an update that changes the
protocol between the processes or the stored data. Updates that are not
listed here need no special order. The update notes of the remote backends
(automatic resume, the read-only runner) are in
[Remote execution backends](remote-backends.md).

## Update to a directory per program (state ownership)

This update gives every program a directory of its own below the state
root and the work root, and moves an existing installation there once, at
the first start of the new `narsor-server`. The move is one way.

### What changes

| Area | Before | After |
|---|---|---|
| Built-in `server.state`, `server.work` | `.`: the `global.root` itself, next to the catalogue | `state` and `work` below `global.root` |
| Server files (`auth/`, `orchestration/`, `materials/`, `observer-queues/`, `agent-harness.json`) | `{state}/` | `{state}/narsor-server/` |
| Old log `narsor.log` | `{state}/narsor.log` | `{state}/narsor-server/narsor.log` (kept, nobody writes it) |
| Discovery and autostart files | `{state}/daemon.json`, `daemon.lock`, `daemon.lock.guard` | `{state}/run/` (the old files are deleted) |
| Local controller, credential side (`credential`, `credential.lock`, `secrets/`, `images/`) | `{state}/local-controller/` | `{state}/narsor-controller-local/` |
| Local controller, storage side (`storage/`, `workloads/`, `readers/`) | `{work}/local-controller/` | `{work}/narsor-controller-local/` |
| Per-task records of the local controller (`tasks/`) | `{work}/local-controller/tasks/` | `{state}/narsor-controller-local/tasks/` |
| Web session (`web-session`, `web-open.html`) | `{state}/` | `{state}/narsor-web/` (the old files are deleted, a new session is made) |

An explicit `server.state` or `server.work` (flag, environment or file)
keeps its value: the files move below the same root. With the built-in
values the old files are in `global.root` and move below
`global.root/state` and `global.root/work`. The catalogue does not move.

### What the migration does

At start, before it reads any of its files, the new server:

1. Takes the old `{old state}/daemon.instance.lock` without waiting. When
   an old daemon holds it, the server stops and names the file. Nothing is
   moved.
2. Reads the old workload records of the local controller
   (`{old work}/local-controller/workloads/` and `readers/`). When a runner
   process (pid and start time) or a runner container still runs, the
   server stops with "Stop all tasks first" and the recovery steps below.
   Nothing is moved.
3. Moves its own files, deletes the stale discovery and autostart files,
   and writes the marker `{state}/narsor-server/layout-migrated`.
4. Starts the local controller with `--legacy-dir <old state>/local-controller`
   and `--legacy-work-dir <old work>/local-controller`. The controller takes
   the old `credential.lock`, records its `controller-id` from the old
   directory (so the Docker images it built earlier stay its own), moves its
   entries, renames the Claude Code sessions of the tasks (see below),
   changes the workspace paths in the stored execution of each task to the
   new storage path (so a stopped task resumes in its moved workspace) and
   writes the marker `{state}/narsor-controller-local/layout-migrated`.
   It removes the old `local-controller/` directories when they are empty.

Within one filesystem each entry is renamed. Across filesystems it is
copied, compared with the source, given its original modes again
(`0600` files, `0700` directories), and then the source is deleted. A
source that is a symbolic link is refused. A failed move stops the program
and names the file; correct the cause and start again. A start after a
crash finishes the move. A program that finds its marker does not migrate
again.

A copy across filesystems can take long. The server waits up to 30 minutes
for its controller while it can migrate, and logs that it waits. A client
started in that time can stop waiting before; start it again when the server
log says that the local controller holds its lease.

**Claude Code sessions.** The workspace of every task moves with its
storage, and Claude Code finds a session by its working directory. In
each task's private HOME on storage, the controller renames
`home/.claude/projects/<key of the old path>` to the key of the new path,
so a stopped task resumes its session. Not covered:

- a task that ran with a host HOME (`controller.local.home: host`) or a
  `CLAUDE_CONFIG_DIR` outside its storage: its sessions are in your own
  `~/.claude/projects/` under the old path, and the task cannot resume them;
- a path longer than 200 characters, which Claude Code shortens with a
  hash: the controller logs a warning and leaves that session.

A task in the Docker work mode sees its storage at the same path inside
its container, so its sessions are renamed the same way.

### Before the update

1. **Back up the state root and the work root** (with the built-in values:
   the whole `global.root`). The migration is one way: the backup is the
   only rollback.
2. **Stop all tasks and the server** (`narsor-server stop`). Make sure no
   runner outlived it (see "Runners that still run").

### Update order

1. Update all binaries together (`narsor-server`, `narsor-controller`,
   `narsor-runner`, `narsor-cli`, `narsor`, `narsor-web`,
   `narsor-observer-worker`): they must sit next to each other and be of
   the same version.
2. Kubernetes: apply the new `controller-rbac.yaml` before you deploy the
   new controller image (see
   [Remote execution backends](remote-backends.md)).
3. Start the server. The migration runs and the server logs what it moved.
   Start the server before you use `narsor-cli secret set` or
   `narsor-cli image refresh`: when you use them first, the migration keeps
   the new `secrets/` and `images/` entries, adds the old entries with other
   names, and leaves each old entry of the same name in the old
   `local-controller/` directory with a warning in the log.
4. Then start the controllers on other machines.

### Runners that still run

When the server refuses with "Stop all tasks first", a runner of the old
installation is still alive. To recover by hand:

1. Kill the runner processes. Each one has the variable
   `NARSOR_LOCAL_WORKLOAD_UID` in its environment, for example:
   `grep -l NARSOR_LOCAL_WORKLOAD_UID /proc/[0-9]*/environ`.
2. Remove the runner containers:
   `docker rm -f $(docker ps -q --filter label=io.narsor.workload)`.
3. Start the server again. After the migration the controller does not
   find these runners, reports them lost, and the control plane resumes
   the tasks (D58).

### Controllers started by hand

A controller started by hand (`narsor-controller local`) now takes
`--state-root` (by default `$XDG_STATE_HOME/narsor`) and `--work-root` in
place of `--credential-file` and `--storage`. Its credential is no longer
found at the old place (`$XDG_STATE_HOME/narsor/workstation-controller/`),
so it must enroll again with a new enrollment code. To keep the old
credential and the task storages, stop the old controller and its runners,
and start the new one once with the old directories:

```bash
narsor-controller local --control-plane https://narsor.example \
  --work-root ~/narsor-laptop \
  --legacy-dir "$XDG_STATE_HOME/narsor/workstation-controller" \
  --legacy-work-dir ~/narsor-laptop
```

The controller does not check for live runners itself: only start it this
way when no runner of the old controller runs.

### After the update

These old entries are left in place for you to delete by hand when you
no longer need them:

- `{old state}/executions/` and `{old state}/launch-specs/`: records of
  the former in-process execution;
- `{old state}/observers/`: records of the removed stored observers;
- anything else left in an old `local-controller/` directory.

### Rollback

There is no rollback by downgrade: an older server does not find the
moved files and starts as a new installation. To go back, stop the new
server, restore the backup of the state and work roots, and install the
older binaries.

## Update to archive only at a terminal state (D80)

This update changes when the owner can archive a task, and what an
archived task keeps. The owner's offer and the owner API take *Archiwizuj*
only when the run reached a state that its workflow marks terminal. The
archive keeps a copy of the step rows (harness, model, effort, tokens and
cost) and of the workspace headers, and the history shows them. The stored
data needs no migration: a task archived before the update shows the last
timeline its runner reported.

### What changes

| Area | Before | After |
|---|---|---|
| Archive offer | In every state of the task. | Only at a terminal state of the workflow. Elsewhere the server refuses the archive with `action_unavailable` and a reason. |
| Archived task | The history says that the storage was deleted. | The timeline read returns the step rows frozen at the archive. |

### Before the update

1. **Find the tasks that you want to archive and that did not finish.**
   After the update, a task that waits, runs, is queued or stopped in a
   step that is not terminal cannot be archived. Archive these tasks
   before the update, or after it move each one to a terminal state with
   a manual transition first (an interrupted task: start it again, then
   move it).

### Update order

1. Update the server (`narsor-server`) first. The offer and the frozen
   run snapshot live in the control plane; the runners and controllers do
   not change.
2. Then update the clients (`narsor`, `narsor-cli`, `narsor-web`). An older
   client follows the new offer, so it shows archive only where the server
   offers it, but it keeps showing "storage deleted" for an archived task
   instead of its step rows.

### Rollback

An older server ignores the frozen run snapshot on the record and offers
archive in every state again. The tasks archived with the new server stay
archived.

## Update to the hierarchical configuration file

This update changes the configuration file from one flat list of keys to a
tree of sections, one section per program (`global`, `server`,
`controller`, `runner`, `tui`, `web`). The environment variables change
with it: `NARSOR_` plus the path of the setting, with `__` between the
levels (`server.oidc.issuer` is `NARSOR_SERVER__OIDC__ISSUER`). See
[Configuration](../user/configuration.md#the-tree-of-sections) for the
tree and the full table of old and new names. The update is a hard
cutover: there is no transition period and no migration tool.

### What changes

| Part | Before | After |
|---|---|---|
| Configuration file | Flat keys at the top level (`server_mode: network`) | Sections (`server: {mode: network}`). A flat key at the top level is an unknown root: it is ignored, not reported as an error. |
| Environment variables | `NARSOR_<KEY>` (`NARSOR_SERVER_MODE`) | `NARSOR_<SECTION>__<KEY>` (`NARSOR_SERVER__MODE`). The old variables are ignored. |
| Directories | Three separate paths; `--root` set all three; a relative path was relative to the working directory | `--root` sets `global.root` only; a relative `server.catalog`, `server.state` or `server.work` is resolved against it, an absolute one is used as is. The built-in values (`./.narsor`, and `.` for each directory) keep the old layout. |
| Observer worker switch | `on` / `off` | `server.observer.enabled: true` / `false` |
| Kubernetes runner arguments | ConfigMap key and variable `NARSOR_AGENT_ARGS` | `NARSOR_RUNNER__AGENT_ARGS`, in the controller ConfigMap and in the admission policy (`deploy/kubernetes/`) |
| Removed settings | `agent_timeout`, `escalation_threshold` | Removed, not moved: nothing read them. An agent state without `timeout` has no limit unless its workflow sets one. |
| Errors | Each program checked the whole file | Each program checks `global` and its own sections only, and refuses to start only on its own errors. The cross-key rules (network mode, observer worker) run before the server starts anything. |
| `config show` | One table of all keys, JSON `version: 1` | One table merged from every program installed next to `narsor-cli`, grouped by section, with the programs that read each setting; JSON `version: 2` |

**An un-migrated installation does not stop.** The old names are ignored,
so a server that still has a flat file or old variables starts on the
built-in values: trusted mode on loopback, the observer worker off, the
default directories. `narsor-server` and `narsor-web` write a warning at
start (a warning, not an error) that names every old key in the file and
every old variable in the environment, with its new name. Do not rely on
the warning: rewrite the configuration before the update.

No file works for both versions: the old programs refuse the section
names as unknown keys, and the new programs ignore the flat keys.

### Before the update

1. **Keep a copy of the old configuration file** for a rollback.
2. **Rewrite the configuration file as a tree.** Use the table in
   [Configuration](../user/configuration.md#old-and-new-names). Write
   `server.observer.enabled: true` for the old `on`. Remove
   `agent_timeout` and `escalation_threshold`. Alternatively, run
   `narsor-cli init` of the new release in a temporary directory: it writes
   a tree in which each commented default names the old key it replaces.
3. **Rename the environment variables** in every service definition, shell
   profile, container spec and ConfigMap that sets `NARSOR_*` settings.
   Variables that are not settings keep their names (`NARSOR_SERVER`,
   `NARSOR_TOKEN*`, `NARSOR_CONTROL_PLANE`, `NARSOR_ENROLLMENT_CODE`, …).
4. **Kubernetes backend:** rename the key `NARSOR_AGENT_ARGS` of the
   controller ConfigMap to `NARSOR_RUNNER__AGENT_ARGS` and apply the new
   `deploy/kubernetes/admission-policy.yaml` together with the new
   controller. An old admission policy refuses the new variable, and a new
   policy refuses the old one.

### Update order

1. Stop the server (`narsor-server`) and `narsor-web`.
2. Install all the new binaries in one directory. `config show` asks each
   program next to `narsor-cli` for its check, so mixed versions give
   wrong results.
3. Put the rewritten configuration file in place and set the renamed
   variables.
4. Run `narsor-cli config show`. Each setting that you set must show the
   source `file` or `env`, not `built-in`; every program must show `ok`
   (or `not installed`). The command exits 1 when a program reports a
   problem.
5. Start the server, the controllers and `narsor-web`. Check that the
   server and `narsor-web` do not write the warning about the old flat
   layout.

### Rollback

Install the old binaries and put the copy of the old configuration file and
the old variables back. The new file does not work with the old programs.

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
