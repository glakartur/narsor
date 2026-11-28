# Updates

This page tells you what to do before and after an update that changes the
protocol between the processes or the stored data. Updates that are not
listed here need no special order. The update notes of the remote backends
(automatic resume, the read-only runner) are in
[Remote execution backends](remote-backends.md).

## Update to the usage-limit wait

This update lets a task wait for an exhausted harness usage limit and go on
in the same agent session after the reset (D88). Before, the stop was a
failed attempt: the retry policy started new sessions that failed at once,
and the task ended in `Error`.

### What changes

| Area | Before | After |
|---|---|---|
| A `claude-code` run that stops on the usage limit | A failed attempt; retries in new sessions, then `Failed`. | The execution is `Paused`; no failed attempt. The same session goes on after the reset. |
| Task state | `Error` after the retries. | `Paused` with the reason "usage limit of <harness> — resumes at HH:MM"; the slot is free. |
| Runner snapshot | — | A new optional field `usage_wait` (harness, reset time). |
| Task record | — | A new optional field `usage_wait` with `resume_at`. Old records read as "no wait". |
| Task history | — | `UsageWaitScheduled`, `UsageWaitResumed`; slot release with the reason `usage_limit`. |
| Reserved idempotency keys | `slot-release:`, `auto-resume:` | Also `usage-resume:`. An owner command with this prefix is refused. |
| Backend `limits` | — | `usage_wait_retry_secs` (default 900) and `usage_wait_margin_secs` (default 60). |

### Update order

Update the server first, then the runners, then the clients. A new runner
that sends `usage_wait` to an older server is ignored by it: the task shows
`Running` with a parked execution and nobody resumes it. An older client
shows a waiting task as a plain `Paused`.

### Rollback

An older server ignores the new fields of the task record. A task that waits
at the rollback stays `Stopped`; resume it by hand.

## Update to scripts that create tasks as the CLI does

This update removes `tasks` from the queue file. A `check` script creates a
task with `narsor-cli task create` and the same arguments as from the
console: the workflow, `--input`, `--repository`, `--branch`, `--ticket`,
`--title`, or all of them as one JSON object with `--stdin`.

### What changes

| Area | Before | After |
|---|---|---|
| Queue file | `tasks`: workflow, workspaces, backend, profile, `mapping`, `task_limit_per_run`, `import_backlog` | No `tasks`. A queue file with it is a problem and does not run. |
| `narsor-cli task create` in a script | Only `--data` with the mapped fields | The options of the console (`WORKFLOW`, `--input`, …), and `--stdin`. `--data` is removed. |
| `narsor-cli task decide` | `--data JSON` or the options | `--stdin` (the same JSON) with the options over it. `--data` is removed. |
| Task limit per run | `tasks.task_limit_per_run`; over it the pair stopped in `needs_attention` | None: the script decides how many tasks it creates. A stored `needs_attention` pair is `active` again. |
| Script input | `import_backlog` | Removed: put it into `condition` when a script needs it. |

### Before the update

For each queue file with `tasks`: remove `tasks`, and change the
`narsor-cli task create` line of its script to name the workflow and to
give the inputs, repositories, ticket and title. Do this together with the
update of the server: the old server refuses `task create` without
`--data`, the new one refuses a queue file with `tasks`.

## Update to the runtime directory

This update moves the runtime catalogue and the component scripts it names
into one directory of the catalogue, `{catalog}/runtime/`:

```text
{catalog}/runtime.yaml   ->  {catalog}/runtime/runtime.yaml
{catalog}/components/    ->  {catalog}/runtime/components/
```

A bare-string script path in `runtime.yaml` (`install`, `version`) is now
relative to the directory of `runtime.yaml`, and must stay inside that
directory. With the file at the catalogue root, that is the same rule as
before, so the usual `components/...` entries do not change. A sealed task
image holds the script text, not the path. So when you move the files
without changing the text, every task keeps its environment identity, and
`narsor-cli task refresh-env` reports no change.

### What changes

| Area | Before | After |
|---|---|---|
| Runtime catalogue | `{catalog}/runtime.yaml` | `{catalog}/runtime/runtime.yaml` (`.yml` also accepted) |
| Component scripts | `{catalog}/components/` | `{catalog}/runtime/components/` |
| Script paths | Relative to the catalogue root | Relative to the directory of `runtime.yaml` |
| Example catalogue | `examples/catalog/runtime.yaml`, `examples/catalog/components/` | `examples/catalog/runtime/` |

### The transition

Until the files move, the server reads `{catalog}/runtime.yaml` at the old
place. It resolves the scripts from the catalogue root and logs one warning
per process with the move to make. A write of the runtime catalogue through
the API goes to the file in use.

When both `{catalog}/runtime.yaml` and `{catalog}/runtime/runtime.yaml`
exist, neither file wins. Task creation and environment refresh fail with an
error that names both files. Keep one of the two, and delete the other.

A later release removes the old place. Its update notes will say so.

### The installer moves the files

For the `local` and `server` roles, the installer moves the files after the
data migration. First it runs `narsor-migrate runtime-catalog`, which only
plans: it reads the same configuration as `narsor-server` (`--config-file`
or `--config`, else the one the server finds), checks for a running
program and for a collision, and prints the moves. Then bash does each move
with `mv -n`: `components/` first, then `runtime.yaml`. Right before each
move, the installer checks again that nothing exists at the target (for
`runtime.yaml`, also `runtime.yml`), and afterwards that the source is gone.
It never copies, deletes or overwrites a file, so it makes no backup and
asks no question. A rerun after a crash between the two moves finishes the
move.

- **Narsor still runs.** An interactive installer offers once more to stop
  it. When you decline, or with `--non-interactive`, the installer skips
  the move, prints in a highlighted line what to do, and continues. Stop
  Narsor, then run the installer again. The same happens when a program
  that the installer did not see holds a lock of the installation.
- **Both layouts exist** (`runtime/runtime.yaml` is already there, or both
  `components/` and `runtime/components/` exist), **or a move fails** (for
  example, the catalogue is read-only, or a file appears at the target
  after the check). The installer prints both paths or the failed move.
  It overwrites nothing. A non-interactive installation then
  fails; an interactive one continues.

### Scripts outside `components/`

A bare-string script outside `components/` (for example
`install: tools/x.sh`) moves with the rest. The installer prints a warning
that names the entry, after the move. After the move, that script is looked up in
`{catalog}/runtime/`, where it is not. A Docker-mode task does not start
until you move the script into `{catalog}/runtime/` (here
`{catalog}/runtime/tools/x.sh`) or make it inline (`{ inline: "…" }`).

### Run it by hand

Run the installer again after you stop Narsor. Or move the files
yourself with a no-clobber `mv`. Stop Narsor first:

```console
$ narsor-migrate runtime-catalog              # print the plan; changes nothing
$ mkdir -p {catalog}/runtime
$ mv -nT {catalog}/components {catalog}/runtime/components   # when it exists
$ mv -nT {catalog}/runtime.yaml {catalog}/runtime/runtime.yaml
```

`mv -n` does not fail when it skips a file, so check that the old path is gone.
Pass `--config FILE` or `--root DIRECTORY` before `runtime-catalog` when the
server uses them. Exit status of `runtime-catalog`: 0 planned, or nothing to
move; 1 the check could not run; 2 usage or configuration error; 3 the
runtime directory already holds what would move; 5 a Narsor program still
runs.

### The catalogue in a ConfigMap

The `runtime.yaml` key of a catalogue ConfigMap now maps to
`runtime/runtime.yaml` (see
[The catalogue in a ConfigMap](remote-backends.md#the-catalogue-in-a-configmap)).
If you keep your own copy of the init script, change that mapping. Also make
the script delete `{catalog}/runtime.yaml` before the copy, as it already
empties `automations/` and `queues/`. Otherwise, on a catalogue that
persists between Pod starts, the old file stays beside the new one and stops
every task. Until you change the mapping, the old place keeps working.

### Rollback

An older version reads only `{catalog}/runtime.yaml`. Before you go back,
move `{catalog}/runtime/runtime.yaml` to `{catalog}/runtime.yaml` and
`{catalog}/runtime/components/` to `{catalog}/components/`.

## Update to the fixed queue order (D84)

This update keeps every task at its place in its backend's queue until the
owner moves it. Before, a resume on a full backend, and the resume that an
answer, a comment or an operator command causes, put the task at the front
of the queue. Now the task goes back to the queue at its own place.

### What changes

| Area | Before | After |
|---|---|---|
| Stored task record | Version 2, with a dense `rank` (1, 2, 3, …). | Version 3, with a sparse `position_key`. |
| Resume on a full backend | The task goes to the front of the queue. | The task keeps its place in the queue. |
| Position shown by the clients (`#n`) | The stored rank, archived tasks included. | Computed by the server among the tasks that are not archived. An archived task shows `#0`. |

The protocol between the server, the controllers and the clients does not
change, so you can update them in any order.

### The migration

The first time the control plane reads a backend that has records below
version 3, it gives each record of that backend a position key in the
current queue order. The order that the owner set before the update stays.
The migration runs under the backend's schedule lock, is safe to stop, and
continues where it stopped at the next read. It writes nothing when every
record is already at version 3.

In a queue that the owner reordered, the migration can give a run of tasks
consecutive keys. The first move into such a run rewrites the keys of some
tasks in it, one time. Later moves there write only the moved task.

### Rollback

There is no rollback by downgrade: an older server refuses a version 3
record. Back up the state directory before the update if you may have to go
back, and restore that backup to roll back.

## Update to the environment refresh

This update lets the owner refresh the environment of a stopped task
(`narsor-cli task refresh-env <task>`, the action *Odśwież środowisko* in
the TUI and the web): the control plane resolves the environment from the
current catalogue again and stores it in the task's launch spec under a new
generation. The workflow, its version, the inputs, the workspace, the
resources, the title and the ticket do not change. The stored data needs no
migration, and a task that you do not refresh works as before.

### What changes

| Area | Before | After |
|---|---|---|
| Environment of a task | Sealed at creation; a change in the catalogue reaches only new tasks. | Sealed at creation; a refresh seals it again from the current catalogue. |
| Runner on existing task storage | Refuses a launch spec whose digest differs from its storage copy (foreign storage). | Accepts a newer spec of the same task when only the environment differs and the spec verifies; refuses all other differences as before. |
| Docker mode, task without a task image | The refusal says to create the task again. | The refusal names `narsor-cli task refresh-env <task>`. |
| Task history | — | A refresh records an `EnvironmentRefreshed` event with the owner, the old and new environment identity and the difference. |

### Update order

1. Update the control plane (`narsor-server`).
2. Update the controllers and the runners (`narsor-controller`,
   `narsor-runner`). On the local backend the runner sits next to the
   controller, so update both binaries together.
3. Update the clients (`narsor-cli`, `narsor`, `narsor-web`). An older client
   does not offer the refresh and shows the new event as an unknown event.

Refresh a task only after its runner is updated. A runner of an older
version does not know the refresh: it finds a launch spec with a new digest
on task storage that holds the old one and refuses it as foreign storage.
The task then fails to start; it starts again after you update the runner.

### Kubernetes: publish the runner in the runtime bundle first

On Kubernetes the runner of a task comes from the runtime bundle sealed in
the task's environment (`runtime.yaml` → `bundle`), not from the
controller's machine. A refresh seals the bundle that `runtime.yaml` names
at that moment. So before you refresh any Kubernetes task:

1. Publish a runtime bundle image that contains the new runner.
2. Set it as `bundle` in `runtime.yaml` (by digest).
3. Then refresh the tasks.

When you refresh with an older bundle in `runtime.yaml`, the refreshed task
gets an older runner, which refuses its storage as above. Set the new bundle
and refresh again.

**Known risk.** A refresh can give a task a newer runtime bundle than the
one that started its execution. There is no documented compatibility rule
for a newer runner that resumes an older execution, so a resume can fail.
This risk is accepted: fix the catalogue (for example, set the bundle back)
and refresh the task again.

### Rollback

An older runner refuses the storage of a task that you refreshed (foreign
storage), so do not roll back the runners while refreshed tasks must run.
Tasks that you did not refresh are not affected.

## Update to a directory per program (state ownership)

This update gives every program a directory of its own below the state
root and the work root. The installer of the new version moves an existing
installation there once with `narsor-migrate`, after a backup. The programs
themselves move nothing: the new `narsor-server` refuses to start while the
old layout is not moved. The move is one way.

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

### The installer runs the migration

For the `local` and `server` roles the installer runs `narsor-migrate`
after it installed the binaries and stopped the running Narsor processes
(see [Installation](installation.md#install-a-role)):

- **Interactive:** `narsor-migrate` prints its plan, runs its checks and
  asks `Back up and migrate now? [y/N]`.
- **`--non-interactive --migrate`:** it migrates without a question
  (`--yes`). A refusal or a failure makes the installation fail.
- **`--non-interactive` without `--migrate`:** the installer prints the plan
  and the command to run; nothing is moved.
- **Narsor still running** (you answered no to stopping it): the installer
  skips the migration with a highlighted message and the command to run.

Until the migration is done, the new `narsor-server` stops at start with a
message that lists the old entries it found and the exact `narsor-migrate`
command. A fresh installation has no old entries and starts normally.

### What `narsor-migrate` does

`narsor-migrate` reads the configuration as `narsor-server` does (`--config
<file>`, `--root <dir>`, the `NARSOR_*` variables and the configuration file
it finds) and works out the old roots from it. One run:

1. **Finds what is left of the old layout.** With nothing there, or with the
   marker `{state}/narsor-server/layout-migrated`, it prints
   `Nothing to migrate.` and exits 0.
2. **Prints the plan:** every entry that moves and its new place, the stale
   files it deletes, the old entries it leaves, what the backup holds, the
   estimated backup size and the free space where it goes.
3. **Checks, before it changes anything,** and refuses (exit 3) when:
   - an old daemon holds `{old state}/daemon.instance.lock`, a new server
     holds `{state}/narsor-server/daemon.instance.lock`, or an old or new
     local controller holds its `credential.lock`;
   - a runner of the old local controller still lives: a process (pid and
     start time in `{old work}/local-controller/workloads/` and `readers/`)
     or a container that `docker` reports as running;
   - a workload record names a container and `docker` cannot answer (no
     `docker`, no daemon, no permission). Check with
     `docker ps --filter label=io.narsor.workload` yourself, remove what
     runs, and run again with `--assume-containers-stopped`;
   - an entry to move is a symbolic link;
   - the backup needs more than the free space on its filesystem (the
     estimate plus 10 %). Choose another place with `--backup-dir`.
4. **Asks for confirmation** on a terminal. Without a terminal it refuses
   (exit 4) unless you pass `--yes`.
5. **Writes the backup** (see below).
6. **Moves the files.** It moves the server's entries by allowlist, splits
   the old `local-controller/` into the controller's state and work
   directories, records the controller's `controller-id` from the old
   directory (so the Docker images it built earlier stay its own), renames
   the Claude Code sessions of the tasks (see below), changes the workspace
   paths in the stored execution of each task to the new storage path (so a
   stopped task resumes in its moved workspace), deletes the stale discovery,
   autostart, instance lock and web session files, and writes the markers
   `{state}/narsor-controller-local/layout-migrated` and, last,
   `{state}/narsor-server/layout-migrated`. It removes the old
   `local-controller/` directories when they are empty.
7. **Prints a summary:** the backup, what moved, what was deleted and what
   is left for you to delete.

Within one filesystem each entry is renamed. Across filesystems it is
copied, compared with the source (entries, bytes, links, modes and
modification times), given its original modes and times again (`0600` files,
`0700` directories), and then the source is deleted. Sockets and FIFOs are
not copied; a warning names each one. A read-only directory of a task (for
example a module cache) is made writable for its owner when the copied
source must be deleted. A failed move stops the run (exit 1) and names the
file; correct the cause and run `narsor-migrate` again. A run after a crash
finishes the move.

`secrets/` and `images/` that a newer `narsor-cli secret set` or
`narsor-cli image refresh` created before the migration are merged: the new
entries stay, the old ones are added, and an old entry with the same name
as a new one stays in the old `local-controller/` directory with a warning.

**Claude Code sessions.** The workspace of every task moves with its
storage, and Claude Code finds a session by its working directory. In
each task's private HOME on storage, the migration renames
`home/.claude/projects/<key of the old path>` to the key of the new path,
so a stopped task resumes its session. Not covered:

- a task that ran with a host HOME (`controller.local.home: host`) or a
  `CLAUDE_CONFIG_DIR` outside its storage: its sessions are in your own
  `~/.claude/projects/` under the old path, and the task cannot resume them;
- a path longer than 200 characters, which Claude Code shortens with a
  hash: a warning names that session and it is left.

A task in the Docker work mode sees its storage at the same path inside
its container, so its sessions are renamed the same way.

### Exit status of `narsor-migrate`

| Status | Meaning |
|---|---|
| 0 | Migrated, nothing to migrate, or a dry run with nothing to refuse |
| 1 | The migration started and stopped; the message names the file. Run it again to finish |
| 2 | Wrong command line or a configuration that does not hold up |
| 3 | Refused before anything changed |
| 4 | Not confirmed; nothing changed |

### The backup

The backup is a zstd-compressed tar archive, written before anything moves:
`<global.root>/backups/layout-migration-<UTC time>.tar.zst` (for example
`~/.narsor/backups/layout-migration-20261122T101500Z.tar.zst`). The
directory is created `0700`, the archive `0600`. `--backup-dir <dir>` puts it
elsewhere; `--no-backup` skips it.

It holds the old-layout entries only, not the whole root: with the built-in
values the old state root is `global.root`, which also holds the catalogue
and the new directories. These entries go in, from the old state root:
`auth/`, `orchestration/`, `materials/`, `observer-queues/`,
`agent-harness.json`, `narsor.log`, `local-controller/`, the daemon files
(`daemon.json`, `daemon.lock`, `daemon.lock.guard`, `daemon.instance.lock`),
the web session files, and the old `executions/`, `launch-specs/` and
`observers/`; from the old work root (when it is another directory) its
`local-controller/`. Modes, times and symbolic links (as links) are kept;
sockets and FIFOs are left out. Each entry is stored under its absolute path
without the leading `/`. After writing, the archive is read back and its
entries counted.

`narsor-cli purge` and `install.sh --uninstall --purge` delete the data
directory with its `backups/`. Copy a backup elsewhere if you want to keep
it.

### Restore the backup

1. Stop every Narsor process (`narsor-server stop`).
2. Remove what the migration made: `{state}/narsor-server/`,
   `{state}/narsor-controller-local/` and `{work}/narsor-controller-local/`
   (with the built-in values: `~/.narsor/state/narsor-server/` and so on).
   Anything the new version wrote there is lost.
3. Extract the archive at `/`; it puts every entry back at its old path:

   ```bash
   tar --zstd -xpf ~/.narsor/backups/layout-migration-20261122T101500Z.tar.zst -C /
   ```

   Without `tar --zstd` (GNU tar older than 1.31):
   `zstd -dc <archive> | tar -xpf - -C /`.
4. Install the older binaries (see "Rollback").

### Run it by hand

```bash
narsor-migrate --dry-run                    # plan and checks only
narsor-migrate                              # asks before it changes anything
narsor-migrate --config /etc/narsor/config.yaml --yes
narsor-migrate --root /srv/narsor --backup-dir /mnt/backup
```

`--root` and `--config` mean what they mean for `narsor-server`. Run it with
the same environment as the server, so that `NARSOR_*` variables give the
same roots.

### Before the update

1. Stop all tasks and the server (`narsor-server stop`), or let the
   installer stop them. Make sure no runner outlived them (see "Runners that
   still run").
2. Make sure the backup fits: `narsor-migrate --dry-run` of the new version
   prints its estimated size and the free space. A separate backup of the
   state and work roots is still a good idea.

### Update order

1. Update all binaries together with the installer (`narsor-server`,
   `narsor-controller`, `narsor-runner`, `narsor-cli`, `narsor`,
   `narsor-web`, `narsor-observer-worker`, `narsor-migrate`): they must sit
   next to each other and be of the same version. The installer runs the
   migration.
2. Kubernetes: apply the new `controller-rbac.yaml` before you deploy the
   new controller image (see
   [Remote execution backends](remote-backends.md)).
3. Start the server.
4. Then migrate and start the controllers on other machines (see
   "Controllers started by hand").

### Runners that still run

When `narsor-migrate` refuses with "Stop all tasks first", a runner of the
old installation is still alive. To recover by hand:

1. Kill the runner processes. Each one has the variable
   `NARSOR_LOCAL_WORKLOAD_UID` in its environment, for example:
   `grep -l NARSOR_LOCAL_WORKLOAD_UID /proc/[0-9]*/environ`.
2. Remove the runner containers:
   `docker rm -f $(docker ps -q --filter label=io.narsor.workload)`.
3. Run `narsor-migrate` again. After the migration the controller does not
   find these runners, reports them lost, and the control plane resumes
   the tasks (D58).

### Controllers started by hand

A controller started by hand (`narsor-controller local`) on another machine
now takes `--state-root` (by default `$XDG_STATE_HOME/narsor`) and
`--work-root` in place of `--credential-file` and `--storage`. Its credential
is no longer found at the old place
(`$XDG_STATE_HOME/narsor/workstation-controller/`). Either:

- **Migrate it.** Install the new version on that machine, stop the old
  controller and its runners, and run `narsor-migrate controller` there with
  the old directories and the new roots. It makes the same checks, a backup
  in `<state root>/backups/` and the same moves as for an installation:

  ```bash
  narsor-migrate controller \
    --legacy-dir "$XDG_STATE_HOME/narsor/workstation-controller" \
    --legacy-work-dir ~/narsor-laptop \
    --work-root ~/narsor-laptop
  narsor-controller local --control-plane https://narsor.example \
    --work-root ~/narsor-laptop
  ```

  `--legacy-dir` is the old credential directory, `--legacy-work-dir` the old
  `--storage` (default: `--legacy-dir`), `--state-root` the new state root
  (default `$XDG_STATE_HOME/narsor`). Its backup holds the entries that move.
- **Enroll it again** with a new enrollment code. Its old task storages are
  not used.

### After the update

These old entries are left in place for you to delete by hand when you
no longer need them (they are in the backup):

- `{old state}/executions/` and `{old state}/launch-specs/`: records of
  the former in-process execution;
- `{old state}/observers/`: records of the removed stored observers;
- anything else left in an old `local-controller/` directory.

### Rollback

There is no rollback by downgrade: an older server does not find the
moved files and starts as a new installation. To go back, stop the new
server, restore the backup (see "Restore the backup"), and install the
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
| Directories | Three separate paths; `--root` set all three; a relative path was relative to the working directory | `--root` sets `global.root` only; a relative `server.catalog`, `server.state` or `server.work` is resolved against it, an absolute one is used as is. The built-in values (`./.narsor`, and `.` for each directory) keep the old layout. (A later update changed the built-in `server.state` and `server.work` to `state` and `work`; see [Update to a directory per program](#update-to-a-directory-per-program-state-ownership).) |
| Observer worker switch | `on` / `off` | `server.observer.enabled: true` / `false` |
| Kubernetes runner arguments | ConfigMap key and variable `NARSOR_AGENT_ARGS` | `NARSOR_RUNNER__AGENT_ARGS`, in the controller ConfigMap and in the admission policy (`deploy/kubernetes/`) |
| Removed settings | `agent_timeout`, `escalation_threshold` | Removed, not moved: nothing read them. An agent state without `timeout` has no limit unless its workflow sets one. |
| Errors | Each program checked the whole file | Each program checks `global` and its own sections only, and refuses to start only on its own errors. The cross-key rules (network mode, observer worker) run before the server starts anything. |
| `config show` | One table of all keys, JSON `version: 1` | One table merged from every program installed next to `narsor-cli`, grouped by section, with the programs that read each setting; JSON `version: 2` |

**An un-migrated installation does not stop.** The old names are ignored,
so a server that still has a flat file or old variables starts on the
built-in values: trusted mode on loopback, the observer worker off, the
default directories. `narsor-server`, `narsor-web`, `narsor`, `narsor-cli`
and `narsor-controller local` write a warning at start (a warning, not an error) that names every old key in the file and
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
   a tree with each setting's environment variable in an `# Env:` comment;
   take the old names from the table in Configuration.
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

> The paths in this section are those of the release that made this update.
> After the [update to a directory per program](#update-to-a-directory-per-program-state-ownership),
> the queue state is in `{state}/narsor-server/observer-queues/`, and old
> observer records are looked for in `observers/` below the old state root.

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
   schedule and condition; a script names the workflow of its tasks in
   `narsor-cli task create`). Change each script to contract version 3.
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
