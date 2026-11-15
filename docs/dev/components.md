# Components and dependencies

This document maps the crates of the workspace, the dependencies between
them at build time, and the processes they become at runtime. For the
decisions behind this structure, see `architecture.md`.

## Crates

| Crate | Responsibility |
|---|---|
| `narsor` (root, `src/`) | The `narsor` (TUI) and `narsor-cli` binaries and the client library they share. |
| `narsor-protocol` | Wire types shared by clients and services: REST API, events, controller protocol, panel forms. |
| `narsor-shared` | Small contracts shared by the processes: API errors, configuration. |
| `narsor-api-client` | HTTP/SSE client transport; discovers a running `narsor-server` or launches one. |
| `narsor-controller-runtime` | Lifecycle helpers shared by the local controller and the `narsor-controller` process. |
| `narsor-controller-client` | The client side of the controller protocol: `ControllerClient`, `RunnerClient`, the wire types, proof-of-possession signing, and the adapter to `narsor-controller-runtime`. |
| `narsor-model` | The model layer: domain types, workflow definitions (schema, loading, templates, validation) and shared configuration. No I/O beyond reading workflow files. |
| `narsor-store` | File-backed storage primitives: state paths, atomic writes, file locks, the execution store, event logs and conversation records. |
| `narsor-exec` | The execution layer: the Engine that runs a workflow, the conductor, task workspaces, the repository registry, task definitions and their sources, and the API projections of an execution. |
| `narsor-engine` | The control plane: orchestration, controller protocol service, auth, observers, materials, environment configuration and launch resolution. Re-exports `narsor-exec`, `narsor-model`, `narsor-store` (as `persistence`) and `narsor-controller-client` under its old module paths. |
| `narsor-server` | The daemon (control plane): the API server, its lifecycle and the child processes it supervises. |
| `narsor-runner` | The isolated task runner: the Engine run against one portable task storage. |
| `narsor-controller` | The environment controller process: `narsor-controller local` and `narsor-controller kubernetes`. |
| `narsor-observer-worker` | The observer worker: claims due observer runs and executes their scripts. |
| `narsor-web` | The browser interface: serves the single-page application and proxies `/control/v1` to a `narsor-server`. |

## Build-time dependencies

Taken from the `[dependencies]` sections of the `Cargo.toml` files. An arrow
means "depends on". Every crate except `narsor-protocol` also depends on
`narsor-protocol` directly; those edges are left out for readability.

```mermaid
graph TD
  narsor["narsor (TUI + CLI)"] --> api[narsor-api-client]
  web[narsor-web] --> api
  obs[narsor-observer-worker] --> api
  server[narsor-server] --> api
  server --> engine[narsor-engine]
  server --> cr[narsor-controller-runtime]
  runner[narsor-runner] --> exec[narsor-exec]
  runner --> cc[narsor-controller-client]
  ctrl[narsor-controller] --> engine
  ctrl --> cc
  ctrl --> cr
  engine --> exec
  engine --> cc
  engine --> cr
  exec --> store[narsor-store]
  exec --> model[narsor-model]
  store --> model
  cc --> model
  cc --> cr
  model --> shared[narsor-shared]
  api --> shared
  cr --> shared
  web --> shared
  narsor --> shared
  server --> shared
  shared --> protocol[narsor-protocol]
```

| Crate | Depends on |
|---|---|
| `narsor` (root) | api-client, protocol, shared |
| `narsor-web` | api-client, protocol, shared |
| `narsor-observer-worker` | api-client, protocol |
| `narsor-server` | api-client, engine, controller-runtime, protocol, shared |
| `narsor-runner` | exec, controller-client, protocol |
| `narsor-controller` | engine, controller-client, controller-runtime, protocol |
| `narsor-engine` | exec, model, store, controller-client, controller-runtime, protocol, shared |
| `narsor-exec` | model, store, protocol, shared |
| `narsor-controller-client` | model, controller-runtime, protocol, shared |
| `narsor-store` | model, protocol |
| `narsor-model` | protocol, shared |
| `narsor-api-client` | protocol, shared |
| `narsor-controller-runtime` | protocol, shared |
| `narsor-shared` | protocol |
| `narsor-protocol` | — |

Rules this graph keeps:

- Clients (`narsor`, `narsor-web`, `narsor-observer-worker`) do not depend on
  `narsor-engine`. They reach the engine only through `narsor-api-client`.
- Only `narsor-server` and `narsor-controller` link `narsor-engine` (the
  control plane). `narsor-runner` links only the execution layer
  (`narsor-exec`) and the protocol client (`narsor-controller-client`).
- `narsor-protocol` has no workspace dependencies.

The root package lists every crate under `[dev-dependencies]`. Only the
integration tests in `tests/` use them, so the graph does not show them.

## Runtime processes

Which process starts which, and how the processes talk to each other.
Solid arrows are process starts; dashed arrows are HTTP calls.

```mermaid
graph TD
  browser([Browser]) -. "HTTP: SPA + session token" .-> web[narsor-web]
  tui["narsor / narsor-cli"] -. "REST + SSE /control/v1" .-> server
  web -. "REST + SSE /control/v1 (proxy)" .-> server
  tui -- "launches when not running (re-exec + setsid)" --> server
  web -- "launches when not running" --> server

  server[["narsor-server (daemon, control plane)"]]
  server -- "starts, enrolls, supervises" --> local[narsor-controller local]
  server -- "starts, supervises (when enabled)" --> obs[narsor-observer-worker]

  local -. "/controller/v1: enroll, lease, heartbeat, commands, acks, reports" .-> server
  k8s[narsor-controller kubernetes] -. "/controller/v1" .-> server

  local -- "one process per task" --> runner[narsor-runner]
  k8s -- "Kubernetes API: one Pod + one PVC per task" --> pod["Pod: narsor-runner"]
  runner -. "/runner/v1: poll, renew, snapshots, usage, materials" .-> server
  pod -. "/runner/v1" .-> server

  runner -- "workflow actions" --> agent["agent harness (claude), action scripts, git"]
  pod -- "workflow actions" --> agent

  obs -. "/observer/v1/worker: claim, result" .-> server
  obs -- "runs" --> script[observer script]
  script -. "polls" .-> ext([Jira / GitHub / GitLab])
  script -- "calls" --> cli2[narsor-cli]
  cli2 -. "/observer/v1/runs: tasks, events" .-> server
```

| From | To | Kind | Channel |
|---|---|---|---|
| `narsor`, `narsor-cli`, `narsor-web` | `narsor-server` | Discover the daemon; launch it when it is not running | process (`api-client/src/api/launch.rs`) |
| `narsor`, `narsor-cli`, `narsor-web` | `narsor-server` | Owner operations and the event stream | REST + SSE, `/control/v1` |
| Browser | `narsor-web` | Application files and the API proxy | HTTP, loopback only |
| `narsor-server` | `narsor-controller local` | Start as a child, enroll, supervise | process (`server/src/local_controller.rs`) |
| `narsor-server` | `narsor-observer-worker` | Start and supervise when the configuration enables it | process (`server/src/observer_worker.rs`) |
| `narsor-controller` (local, kubernetes) | `narsor-server` | Enroll, lease, heartbeat, commands, acks, reports | HTTP, `/controller/v1` |
| `narsor-controller local` | `narsor-runner` | One runner process per task | process (`narsor-controller/src/local/backend.rs`) |
| `narsor-controller kubernetes` | Kubernetes | One Pod (command `narsor-runner`) and one PVC per task | Kubernetes API (`controller/src/kubernetes/manifest.rs`) |
| `narsor-runner` | `narsor-server` | Poll, renew, snapshots, usage, materials | HTTP, `/runner/v1` |
| `narsor-runner` | agent harness, scripts, git | Execute workflow actions | process (`engine/src/engine/actions/`) |
| `narsor-observer-worker` | `narsor-server` | Claim due runs, report results | HTTP, `/observer/v1/worker` |
| Observer script | `narsor-cli` → `narsor-server` | Create tasks, deliver events to waiting tasks | process, then HTTP `/observer/v1/runs` |

Properties of the runtime:

- `narsor-server` is the only hub. Every other process talks to it over
  HTTP. The server does not call controllers or runners; they pull work
  from it (poll, commands).
- The `local` backend works like the `kubernetes` backend: its controller is
  a separate process that enrolls through the same `/controller/v1`
  protocol.
- Only `narsor-runner` executes workflows. The daemon drives no execution:
  its `/executions` routes read what runners keep and hand answers to the
  task of the execution.
