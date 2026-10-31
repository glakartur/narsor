# Remote tasks and backends

How the control plane runs tasks on registered backends. Operating a server and its backends is covered in [Remote execution backends](../operations/remote-backends.md).

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
narsor-cli usage [--json]                         # reported Claude/Copilot limits (in memory only; expires after 15 min, reset or daemon restart)
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
[`docs/operations/remote-backends.md`](../operations/remote-backends.md).
