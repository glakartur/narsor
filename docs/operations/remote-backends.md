# Remote execution backends — operations guide

This guide is for the person who installs and runs a Narsor control plane
with remote backends (Kubernetes first), and for the developer who uses it
from the CLI and the TUI. It describes what the code in this repository does
today; where something is not supported yet it says so.

Contents:

1. [Architecture](#1-architecture)
2. [Server modes: trusted and network](#2-server-modes-trusted-and-network)
3. [Catalogue: backends, profiles, runtime, repositories](#3-catalogue-backends-profiles-runtime-repositories)
4. [Secrets](#4-secrets)
5. [Installing the Kubernetes backend](#5-installing-the-kubernetes-backend)
6. [Enrolling a controller](#6-enrolling-a-controller)
7. [Controller credentials: rotation and compromise](#7-controller-credentials-rotation-and-compromise)
8. [Image contracts](#8-image-contracts)
9. [Task lifecycle](#9-task-lifecycle)
10. [Scheduling](#10-scheduling)
11. [Using it: CLI and TUI](#11-using-it-cli-and-tui)
12. [Smoke tests](#12-smoke-tests)
13. [Known limitations](#13-known-limitations)

---

## 1. Architecture

```text
 TUI / CLI / Web ──REST + SSE──▶ control plane (narsor daemon)
                                    │  owner API      /control/v1, /auth/v1
                                    │  controller API /controller/v1
                                    │  runner API     /runner/v1
                                    │
                                    └─ one durable volume: orchestration records,
                                       event logs, controller identities, owner
 controller (next to the backend) ──outbound HTTPS──▶ control plane
   └─ native API ─▶ Kubernetes: one Pod + one PVC per task
                      init container: copies the runtime bundle to /opt/narsor
                      main container: the repository's base image, runs
                                      /opt/narsor/bin/narsor runner
                      runner ──outbound HTTPS──▶ control plane (/runner/v1)
```

| Part | Holds | Never holds |
| --- | --- | --- |
| **Control plane** (`narsor daemon`) | The authoritative orchestration state: task assignment, desired lifecycle and generation, owner commands, the scheduler's queue, controller leases, the *last observed* runner and backend summaries, a durable per-task event log | Workspace files, raw conversation transcripts, secret values |
| **Controller** (`narsor controller`) | Its own identity (`nsk.` credential) and the backend's native credentials | Any user token; any task state of its own |
| **Runner** (`narsor runner`, inside the task Pod) | The task's real work state on **task storage** (the PVC): engine files, workspace, private `$HOME`, conversation records | Anything central — it reports cursors and summaries only |
| **Task storage** | The authoritative work state of one task, surviving pause/resume | It is not a backup; archive deletes it |

Rules that follow from this split:

* Clients talk to the control plane only. No request a client makes carries
  a backend address, a runner endpoint or a backend credential; the control
  plane relays conversation pages and input to the runner.
* Controllers and runners connect **outbound** to the control plane. Nothing
  connects into a task Pod; the controller never uses `exec`, `attach` or
  `port-forward`.
* Losing the control plane does not stop a running task. The runner keeps
  working and reports a later cursor when it can reach the control plane
  again; meanwhile clients see the last known state marked stale.
* The control plane has one writer and one durable volume. No external
  database, queue or object store is needed. Run exactly one control plane
  per state directory.

The daemon that runs the local `/executions` engine is the same process that
serves the control plane. The `local` backend adapter runs tasks through the
local engine on the same machine; remote tasks on any backend use the same
owner API.

## 2. Server modes: trusted and network

The mode is a setting, never inferred (`narsor config show` lists it). A
configuration that does not hold up stops the daemon with a message naming
every problem.

| | `trusted` (default) | `network` |
| --- | --- | --- |
| For | one person on one machine | a shared, reachable control plane |
| Listens on | loopback only; a wildcard, IPv4-mapped or non-loopback bind is refused | any address, behind a TLS-terminating proxy |
| Users prove | the random per-process token in the `0600` discovery file `{state}/daemon.json` | an OIDC access token of the configured issuer and audience |
| Owner | the local token holder | one OIDC identity `(issuer, subject)` |

### Trusted mode

Nothing to configure. `narsor` commands discover the daemon (or start one)
through the discovery file, which must belong to you with mode `0600`.

### Network mode

```yaml
# ~/.config/narsor/config.yaml (or NARSOR_<KEY> environment variables)
server_mode: network
server_bind: 0.0.0.0:8080            # what the proxy connects to
oidc_issuer: https://login.example.com/realms/dev
oidc_audience: narsor-api            # the token's `aud` must contain this
owner_subject: ""                    # optional: bind the owner by config
tls_termination: proxy
trusted_proxies:                     # IPs or CIDRs of the proxy; never "everything"
  - 10.0.0.12
cors_allowed_origins: []             # browser origins allowed to call the API
```

**OIDC.** Tokens are validated against the issuer's discovery document and
JWKS (refreshed on an unknown key id, rate-limited), with exact issuer and
audience, an asymmetric algorithm allow list (no `none`, no HMAC), signature,
`exp`/`nbf` and a 30 s leeway. The owner is identified by the immutable pair
`(iss, sub)` — never by e-mail or name.

**Owner.** Either set `owner_subject` (the owner is bound at startup), or
leave it empty: the daemon then prints a one-time **claim code** and stores it
`0600` at `{state}/auth/claim-code`. The first user who presents a valid
token together with the code owns the installation:

```bash
export NARSOR_SERVER=https://narsor.example.com
export NARSOR_TOKEN_FILE=~/.cache/narsor/token     # see "Tokens" below
narsor auth claim <code>
narsor auth whoami
```

The claim is atomic and the code is spent by it; every other identity gets
`403` on every owner route. In v1 the owner is also the operator (controller
enrollment and revocation).

**Trusted proxy and TLS.** In network mode the daemon speaks plain HTTP and
accepts requests only from `trusted_proxies`, and only when the proxy says
the client spoke HTTPS. The proxy must:

* terminate TLS and set `X-Forwarded-Proto: https` on **every hop** — with a
  chain of proxies each one appends, and the daemon accepts the request only
  when every listed scheme is `https`;
* not rewrite the path, the query or the body (controller requests are
  signed over method, path with query, and body);
* pass the `Authorization` header unchanged;
* allow long-lived responses without buffering for
  `GET /control/v1/events/stream` (SSE): disable response buffering and use a
  read timeout well above the 15 s keep-alive interval. A stream also ends
  when the token it was opened with expires; clients reconnect with a fresh
  token and resume from their cursor.

**CORS.** Empty `cors_allowed_origins` sends no CORS headers at all, so
browsers refuse every cross-origin call. List exact origins
(`https://narsor-web.example.com`) only for a web client you run.

**Tokens for clients.** The CLI and TUI never take a token on the command
line. They read, in this order: `--token-file` / `NARSOR_TOKEN_FILE` (a file
your token helper keeps fresh), `--token-command` / `NARSOR_TOKEN_COMMAND`
(a command printing a token, e.g. `oidc-token narsor`), or `NARSOR_TOKEN`
(a fixed token). After a `401` the file or command is read again and the
request retried once; a `403` (not the owner, or not through the proxy) is
never retried. The server URL must be `https://` (plain `http://` is
accepted for a loopback host only).

## 3. Catalogue: backends, profiles, runtime, repositories

Everything below lives under the catalogue directory (`catalog_dir`) of the
control plane and is read when a task is **created**: the result is sealed
into the task's immutable launch spec and never re-read for that task.

| What | File |
| --- | --- |
| Backends | `{catalog}/backends/{id}.yaml` |
| Profiles | `{catalog}/profiles/{id}.yaml` |
| Runtime bundle and tool images | `{catalog}/runtime.yaml` |
| Repository base image | `{catalog}/repositories/{id}.yaml`, key `base_image` |
| Workflow image | workflow `globals.image` |
| Default backend / profile | settings `default_backend` (default `local`), `default_profile` (default `default`); the owner can override the backend at runtime |

### Backend

```yaml
# {catalog}/backends/k8s.yaml
kind: kubernetes            # kubernetes | local | fake
trust_scope: dedicated      # workstation | dedicated | shared
capabilities:
  pause_resume: true
  persistent_storage: true
  archive: true
limits:
  max_concurrent_tasks: 3   # slots; see "Scheduling"
  default_resources: { cpu_millicores: 1000, memory_mib: 2048 }
  max_resources:     { cpu_millicores: 4000, memory_mib: 8192 }
harnesses: [claude-code, codex]   # optional allow list
secret_delivery: platform         # none | platform
secrets:
  - name: github-token            # what profiles and repositories refer to
    tool: gh
    key: narsor-github/token      # Kubernetes: <secret-name>/<key> in the task namespace
    delivery: !env GH_TOKEN       # or: !file .config/gh/token  (relative to the private HOME)
```

### Profile

```yaml
# {catalog}/profiles/default.yaml
version: 3                 # bump on every change: it is part of the environment identity
harness: claude-code       # optional
settings: {}               # allowed settings
skills: []
tools: [gh]                # tool images from runtime.yaml
secrets: [github-token]    # names of backend secrets this profile may use
```

### Runtime catalogue

```yaml
# {catalog}/runtime.yaml
bundle: ghcr.io/narsor/runtime@sha256:<64 hex>
tools:
  gh: ghcr.io/narsor/tools/gh@sha256:<64 hex>
```

### Repository base image and digest policy

Every repository used by a remote task needs a base image; a task without one
is refused before anything is queued.

```yaml
# {catalog}/repositories/api.yaml (narsor repo add …, then edit)
remote: https://github.com/acme/api.git
base_image: ghcr.io/acme/api-dev@sha256:<64 hex>      # pinned (default policy)
# or, to let the control plane resolve a tag once at task creation:
# base_image:
#   reference: ghcr.io/acme/api-dev:2026.09
#   digest_policy: resolve
git_credential: github-token     # optional: backend secret used for the clone
```

A workflow's `globals.image` overrides the repository image. Digests are
recorded in the task's `ResolvedEnvironment`; a later change of any
component affects only tasks created afterwards. Two tasks with the same
images, bundle, tools, profile and harness have the same
`environment_identity`.

## 4. Secrets

* The control plane stores **references only** (`name`, `tool`, `key`,
  `delivery`); no API response, event, log or launch spec carries a value.
* Delivery is either an environment variable of the runner process
  (`!env NAME`; `PATH`, `HOME`, `USER`, `SHELL` are reserved) or a file under
  the task's private `$HOME` (`!file relative/path`).
* On Kubernetes the values are ordinary Secrets in the user's dedicated
  Narsor namespace (`narsor-user` in the samples). The controller can
  *create* only its per-incarnation launch Secrets and can neither read nor
  list Secrets; which Secrets a task Pod may mount is fixed by the admission
  policy's `allowedSecrets` (`secret-name/key` pairs).
* Secrets are mounted read-only and recreated on every Pod start; they are
  never copied into the workspace, the task storage or the control plane.
  Their validity and rotation are the owner's and the tool provider's
  responsibility.

## 5. Installing the Kubernetes backend

Requirements: Kubernetes **1.30 or newer** (ValidatingAdmissionPolicy v1),
Pod Security Admission, a StorageClass dedicated to task volumes with
`reclaimPolicy: Delete`, and egress from task Pods and from the controller to
the control plane over HTTPS.

The sample manifests install one independent Narsor instance into one
namespace, `narsor-user`. Before each installation, replace every occurrence
of `narsor-user` with a unique namespace identifier; this changes the
namespace, each binding's immutable namespace selector and parameter
namespace, and the four per-instance cluster-scoped binding names
(`narsor-user-task-pods`, `narsor-user-task-storage`,
`narsor-user-controller-delete-guard` and
`narsor-user-controller-storageclasses`). Do not make multiple people Rancher
Project Owners of a single Project containing these namespaces: that grants
them access across the Project. Use one Rancher Project per user, or
namespace-scoped RBAC instead.

The three `ValidatingAdmissionPolicy` objects are shared cluster policy and
must be installed by a cluster administrator once. Each instance's
cluster-scoped bindings are also installed by that administrator. A binding
covers its user namespace, but its policy is evaluated only for requests made
by that namespace's `narsor-controller` ServiceAccount. Thus the controller
and its state PVC can share the namespace with tasks without trusting labels
that a compromised controller could omit.

Apply the sample instance **in this order**:

```bash
kubectl apply -f deploy/kubernetes/namespaces.yaml
kubectl apply -f deploy/kubernetes/workload-serviceaccount.yaml # narsor-task: no token, no RBAC
kubectl apply -f deploy/kubernetes/controller-rbac.yaml        # namespaced; no exec/attach/port-forward
kubectl apply -f deploy/kubernetes/admission-policy.yaml       # parameters, policies and per-instance bindings
kubectl apply -f deploy/kubernetes/controller.yaml             # backend config, state PVC and Deployment
```

The admission parameter ConfigMap must exist before the bindings reference
it, and the controller must start only once the policies are enforced. An
operator who is only a Rancher Project Owner needs the cluster administrator
to create the shared policies, the per-user admission bindings and the
StorageClass read binding.

**One StorageClass name everywhere.** The dedicated class (here
`narsor-delete`) must have `reclaimPolicy: Delete`, because archive must
destroy the task storage. Use the same name in:

* `backend.yaml` → `storage_class` (controller ConfigMap);
* `narsor-admission` ConfigMap → `storageClass`.

The controller checks the class's reclaim policy before it creates or
deletes a PVC; a class that retains data leaves archive visibly failed
rather than claiming success.

**Keep `backend.yaml` and `narsor-admission` in step:**

| backend.yaml | narsor-admission | Note |
| --- | --- | --- |
| `workload_service_account` | `workloadServiceAccount` | `narsor-task` |
| `storage_class` | `storageClass` | reclaimPolicy Delete |
| `allowed_registries` | `allowedRegistries` | comma-separated prefixes, **each ending in `/`** |
| `allowed_secrets` | `allowedSecrets` | `secret-name/key`, matching the backend record's `secrets[].key` |

Other `backend.yaml` keys: `namespace`, `storage_mib` (PVC size, default
10240), `run_as_user` / `run_as_group` / `fs_group` (default 1000),
`start_timeout_secs` (default 900: how long a Pod may take to report ready
before it counts as lost).

Set in `controller.yaml`: the Narsor release image **by digest**,
`NARSOR_CONTROL_PLANE` (the URL the controller uses) and, when task Pods
reach the control plane under another URL, `NARSOR_RUNNER_CONTROL_PLANE`.
Task Pods need network egress to that URL.

## 6. Enrolling a controller

Each controller has its own identity scoped to one backend.

1. Register the backend in the catalogue (`{catalog}/backends/k8s.yaml`).
2. As the owner, create a one-time code (shown once, expires, default 15 min):

   ```bash
   narsor controller enroll-code k8s --name prod-cluster
   ```

3. Hand it to the controller (first start only):

   ```bash
   kubectl -n narsor-user create secret generic narsor-controller-enrollment \
     --from-literal=code=<code>
   kubectl apply -f deploy/kubernetes/controller.yaml
   ```

4. On its first start the controller generates an Ed25519 key, redeems the
   code by proving possession of it, and writes its credential
   `nsk.<controller-id>.<seed>` with mode `0600` to its state volume. Later
   starts load the file and ignore any code still in the environment.
5. Delete the enrollment Secret, then check:

   ```bash
   kubectl -n narsor-user delete secret narsor-controller-enrollment
   narsor controller list        # the identity holds the backend's lease
   ```

Every controller request is signed (method, path, query, body, timestamp,
nonce); the control plane and the controller clocks must agree within
**±60 seconds**.

## 7. Controller credentials: rotation and compromise

* **Rotation.** The controller can replace its key
  (`POST /controller/v1/key`, signed with the old key and proven by the new
  one); the old key stops working at once. Re-enrolling with a new code and
  revoking the old identity is equally valid.
* **Revocation.** `narsor controller revoke <controller-id>` ends the
  identity and fences its lease immediately; queued tasks of that backend
  show `controller offline` until another controller holds the lease.

**If a controller is compromised:**

1. Revoke its identity: `narsor controller revoke <id>`.
2. Rotate every Secret the admission policy lets task Pods mount
   (`allowedSecrets`) — a controller can create Pods, and Pod creation is a
   secret-injection capability even without `get secret`.
3. Rotate the controller's Kubernetes service-account token (delete the
   Pod / token Secret so a new projected token is issued) and review the
   namespace's audit log.
4. Enroll a fresh controller (section 6).

## 8. Image contracts

### Runtime bundle image

The bundle image (`runtime.yaml` → `bundle`) carries everything Narsor runs in
a task under `/narsor`: at least `/narsor/bin/narsor`, plus the supported
harnesses, their portable dependencies and skills. The init container runs
`cp -a /narsor/. /opt/narsor/`, so the image needs `cp` and the payload must be
self-contained (statically linked or carrying its own loader and libraries).
Tool images are copied the same way under `/opt/narsor-tools`.

### Repository base image

The main container runs the repository's own image with its command replaced
by `/opt/narsor/bin/narsor runner`. The runner checks the contract before any
work and reports a readable `base_image_contract` problem otherwise:

* supported platform and architecture, `/bin/sh` and basic tools;
* `git` and a CA bundle (`/etc/ssl/certs/ca-certificates.crt` or equivalent);
* runs as **non-root** — by default uid/gid 1000 (`run_as_user`,
  `run_as_group`, `fs_group`);
* a writable mounted `$HOME` (the runner sets a private HOME on task storage);
* the start command can be overridden.

Task start never installs packages and never logs in interactively.

**Fixed `PATH`.** Kubernetes cannot append to an image's own `PATH`, so the
main container's `PATH` is spelled out: the runtime bundle and tool
directories first, then
`/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin`. Tools an image
installs elsewhere (e.g. `/opt/conda/bin`) are not on `PATH` unless the
workflow uses absolute paths.

**Per-step images** (a state's own `image`) are not supported on the
Kubernetes backend in v1: one task runs in one container on one image.

## 9. Task lifecycle

```text
Queued ─▶ Provisioning ─▶ Running ─▶ PauseRequested ─▶ Paused ─▶ (resume) Provisioning
                             │
                             └─▶ Interrupted / Unknown ─▶ (resume or start) Provisioning
any state ─▶ Archiving ─▶ Archived
```

* **Pause** asks the runner for a safe point; only after it acknowledges does
  the controller delete the Pod. The PVC — workspace, engine state, private
  `$HOME`, pinned harness — stays.
* **Resume** creates a new Pod on the same PVC; nothing is cloned again.
* **Interrupted / Unknown**: the workload disappeared (a Pod that failed or
  was deleted, or Pod phase `Unknown`). Narsor **never restarts it on its
  own**; the owner decides: `resume`, `start` (afresh) or `archive`.
* **Archive** stops everything and deletes the Pod **and the PVC**. It wins
  over every other state and cannot be undone; there is no automatic
  retention cleanup.
* While a task is paused, interrupted or archived its conversation is
  explicitly unavailable; clients show the last safe summary and its time.
  Observations that are no longer current are shown as stale with their age,
  never turned into another state.

## 10. Scheduling

* Each backend has `max_concurrent_tasks` slots. Slots are held by tasks in
  `Provisioning`, `Running`, `PauseRequested`, `Interrupted`, `Unknown`,
  `Archiving`, and by an admitted `Queued` task. `Paused` and `Archived` hold
  none — so an interrupted task still blocks a slot until the owner resumes or
  archives it.
* Every task has a position (`rank`) in its backend's queue, shown as `#n` by
  `narsor task list`. A new task joins the end; waiting tasks are admitted in
  that order. A queued task is reordered with `narsor task move <task>
  --before <other> | --after <other> | --top | --bottom`, only while it is
  queued and conditional on its generation. Admitted and finished tasks keep
  their place in the list. Nothing running is ever preempted. Tasks stored by
  an older version, which had a `priority`, are numbered once from the old
  order (priority, then creation) the first time the control plane reads them.
* `interrupt`, `transition` and `restart-step` are operator commands for the
  runner of a `Running` task (`POST /control/v1/tasks/:id/operator-commands`).
  They are queued as events (`OperatorCommandQueued`), handed to the runner by
  its poll until it acknowledges, carried out by the runner under the Engine's
  own claim on the execution, and settled with `OperatorCommandResolved`
  (applied, or rejected with the reason). A command for a task that is not
  `Running`, whose runner is silent, or whose generation moved on is refused
  at once. A `transition` names a state a declared transition leads to; an
  override (any state) needs the current state's `allow_manual_override` and a
  `--reason`, which is recorded with the owner in the task's events. The CLI
  waits for the verdict and, past `--wait-secs`, reports the command as
  queued.
* A queued task shows why it waits: `waiting for capacity` (position,
  slots in use / limit), `controller offline` (no controller holds the lease),
  `backend unknown`, or `admitted` (starting).
* Resuming a paused task needs a free slot; on a full backend the resume is
  refused with `backend_at_capacity` — the task stays paused, try again later.
* The default backend applies to tasks created afterwards only; existing
  tasks never move.

## 11. Using it: CLI and TUI

Without `--server` the commands use the trusted local daemon (started when
none runs). With `--server` (or `NARSOR_SERVER`) they use a remote control
plane and a token (section 2).

```bash
narsor backend default                       # get; also: set <backend>, reset
narsor task create implement-feature --input ticket=NAR-1 \
  [--backend k8s] [--profile default] [--harness codex] \
  [--repository api --commit <sha>] [--workflow-version 1] [--task-id my-id]
narsor task list
narsor task show <task>
narsor task pause|resume|start|archive <task> [--expected-generation N]
narsor task move <task> --before <other> | --after <other> | --top | --bottom
narsor task input <task> --field reason=ok [--label approved] [--comment …]
narsor task interrupt <task> [--wait-secs N]
narsor task transition <task> <state> [--override --reason <why>] [--wait-secs N]
narsor task restart-step <task> [--note <text>] [--failure-key K] [--wait-secs N]
narsor task conversation <task> [<attempt>] [--from N]
narsor task events <task> [--follow]
narsor task watch [--cursor C]               # every task, reconnecting from the cursor
narsor task materials upload <task> <path>... [--id ID] [--label T] [--ttl-secs N]
narsor task materials list|show <task> [<material>]
narsor task materials download <task> <material> <dest-dir> [--force]
narsor task materials delete <task> <material>
narsor auth whoami | claim <code>
narsor controller enroll-code <backend> | list | revoke <id>
narsor tui --remote                          # implied by --server
```

In the TUI's remote mode: `j/k` move, `Enter` opens a task, `p` pause, `u`
resume, `s` start afresh, `a` archive (asks first), `i` answer the pending input, `c`/`n`/`[`/`]` read the conversation
through the control plane, `b` default backend, `r` refresh, `?` help.

### Task materials

A *material* is a one-off, bounded set of regular files attached to a task:
input you hand the agent (a few files, a snapshot of a local folder) and
small documents the task leaves for you. It is a copy, never a live mount of
your machine, and it never becomes a commit in the product repository.

* **Upload.** `narsor task materials upload <task> <path>...` snapshots the
  given files and folders once: each file under its own name, each folder's
  content under the folder's name. Symbolic links are never followed; links,
  sockets, FIFOs, devices and `.git` directories inside a folder are skipped
  and listed on stderr; a link or special file named explicitly is refused.
  Repeat an upload safely with `--id`: the same id with the same files
  answers with the existing material, with other files it is refused.
* **Delivery.** The task's runner learns about the material on its next
  long-poll, downloads manifest and blobs with its own incarnation-bound
  credential, verifies every SHA-256, and writes the files atomically to
  `{task storage}/materials/<material>/` — beside the workspace, never in
  it. The agent finds that directory in `$NARSOR_MATERIALS_DIR`. A material
  uploaded to a queued or paused task waits for the next runner.
* **Documents back.** Every folder the task writes into `$NARSOR_OUTBOX_DIR`
  (`{task storage}/outbox/<name>/`) is published as a material labelled
  `<name>` once no engine step runs, then removed from the outbox. A folder
  the control plane refuses is renamed `<name>.rejected`. Download it with
  `narsor task materials download <task> <material> <dest-dir>`: it writes
  only inside `<dest-dir>`, never through a symbolic link, and refuses to
  overwrite an existing file unless `--force` (which still never replaces a
  link or a directory).
* **Expiry.** A material lives 7 days unless `--ttl-secs` says otherwise
  (at most 30 days), or until you delete it. Deleting or expiring removes
  the control plane's copy; a copy already delivered to task storage stays
  there until the task is archived. Archive removes all of the task's
  materials, and an archived task takes no new ones.

Limits (per material unless stated): 1000 files, 16 MiB per file, 64 MiB in
total (8 MiB for a document the task publishes), 32 materials and 256 MiB per
task. Paths must be normalized relative paths: no absolute paths, `..`, `.`,
empty components, backslashes, colons, control characters or `.git`
components, at most 1024 bytes and 32 levels, and no two paths that differ
only in letter case.

Where the bytes live: at the control plane under `{state}/materials/` (one
directory per task and material, blobs named by their SHA-256), outside task
records, event logs and the SSE stream — events say only
`material <id> uploaded|delivered|published|deleted|expired` with a file
count and a byte total. An upload is staged and renamed into place only when
complete and verified; anything a crash leaves half-done is removed at the
next daemon start.

API (owner, behind the owner gate; another owner's task is a `404`):

| method | path | |
| --- | --- | --- |
| `POST` | `/control/v1/tasks/:task/materials` | upload (`application/vnd.narsor.material-bundle`) |
| `GET` | `/control/v1/tasks/:task/materials` | list |
| `GET` | `/control/v1/tasks/:task/materials/:material` | manifest |
| `GET` | `/control/v1/tasks/:task/materials/:material/blobs/:sha256` | one blob |
| `DELETE` | `/control/v1/tasks/:task/materials/:material` | delete |

Runner (behind the runner credential, its own task only):
`GET /runner/v1/materials/:material`, `GET …/blobs/:sha256`,
`POST …/ack`, and `POST /runner/v1/materials` to publish.

## 12. Smoke tests

The ordinary test suite needs no cluster. Against a real cluster:

1. **Reachability and StorageClass check** (creates nothing):

   ```bash
   export NARSOR_K8S_SMOKE_API_SERVER=https://<api-server>
   export NARSOR_K8S_SMOKE_NAMESPACE=<disposable namespace>
   export NARSOR_K8S_SMOKE_TOKEN_FILE=<token with the controller's Role>
   export NARSOR_K8S_SMOKE_CA_FILE=<cluster CA PEM>
   export NARSOR_K8S_SMOKE_STORAGE_CLASS=narsor-delete
   cargo test --test kubernetes_adapter live_cluster_smoke -- --ignored
   ```

2. **Full cycle, by hand, in a disposable namespace** (before exposing a
   network deployment): install section 5 with the namespace and StorageClass
   names changed, enroll a controller, then

   ```bash
   narsor task create <workflow> --backend <backend> --task-id smoke-1
   narsor task show smoke-1          # Running; one Pod and one PVC labelled for the task
   narsor task pause smoke-1         # Paused; the Pod is gone, the PVC remains
   narsor task resume smoke-1        # Running again on the same PVC
   narsor task archive smoke-1       # Archived; Pod and PVC deleted
   kubectl -n <ns> get pods,pvc -l app.kubernetes.io/managed-by=narsor
   ```

   Also check: a second identity cannot read tasks (`403`), a revoked
   controller is fenced, and a non-proxied or non-https request is refused.
   Delete only resources labelled for the test, then the namespace.

## 13. Known limitations

* One control plane per installation; no HA, no teams — one owner, who is
  also the operator.
* Kubernetes is the only production adapter; `local` and `fake` exist for the
  local path and tests. No local Docker or cloud adapters.
* No per-step images on Kubernetes; a fixed `PATH` in the main container.
* Narsor does not isolate task egress; restrict it with NetworkPolicies or
  the backend's own mechanisms.
* No workspace revisions, local download/upload of a workspace, or central
  replication of task storage. Task storage is not a backup.
* Task materials are one-off copies, bounded in size (section 11); there is
  no synchronization, and no removal of a copy already delivered to task
  storage short of archiving the task.
* No automatic restart after workload loss and no automatic retention
  cleanup — both are owner decisions by design.
* OIDC device-code login is not built in: use a token helper that writes a
  file or prints a token.
* The TUI's remote mode is a separate mode (`narsor tui --remote`); the local
  `/executions` interface is unchanged.
