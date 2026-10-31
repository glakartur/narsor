# Configuration

The configuration file, how settings are layered, and where Narsor keeps its data on disk. Architectural background is in [the architecture decisions](../dev/architecture.md).

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
