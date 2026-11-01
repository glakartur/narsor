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
14. [A computer as a backend](#14-a-computer-as-a-backend)

---

## 1. Architecture

```text
 TUI / CLI / Web ──REST + SSE──▶ control plane (narsor-server)
                                    │  owner API      /control/v1, /auth/v1
                                    │  controller API /controller/v1
                                    │  runner API     /runner/v1
                                    │
                                    └─ one durable volume: orchestration records,
                                       event logs, controller identities, owner
 controller (next to the backend) ──outbound HTTPS──▶ control plane
   ├─ narsor-controller local (trusted mode: the daemon's child)
   │    └─ one narsor-runner process per task on this machine
   └─ narsor-controller kubernetes ─▶ native API ─▶ one Pod + one PVC per task
                      init container: copies the runtime bundle to /opt/narsor
                      main container: the repository's base image, runs
                                      /opt/narsor/bin/narsor-runner
                      runner ──outbound HTTPS──▶ control plane (/runner/v1)
```

| Part | Holds | Never holds |
| --- | --- | --- |
| **Control plane** (`narsor-server`) | The authoritative orchestration state: task assignment, desired lifecycle and generation, owner commands, the scheduler's queue, controller leases, the *last observed* runner and backend summaries, a durable per-task event log | Workspace files, raw conversation transcripts, secret values |
| **Controller** (`narsor-controller`) | Its own identity (`nsk.` credential) and the backend's native credentials | Any user token; any task state of its own |
| **Runner** (`narsor-runner`, inside the task Pod) | The task's real work state on **task storage** (the PVC): engine files, workspace, private `$HOME`, conversation records | Anything central — it reports cursors and summaries only |
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
serves the control plane. The `local` backend has a controller like every
other backend: `narsor-controller local`, a separate process that enrolls and
talks `/controller/v1` over HTTP. In trusted mode the daemon starts it as its
child, enrolls it on the first start and supervises it:

* the daemon becomes discoverable (`daemon.json`) only after this controller
  holds a lease of its own — a newer `epoch` than the one before its start;
  it gives up after 10 s, and the start fails with the last lines the
  controller wrote;
* the controller gets every setting as a flag (`--no-config`) and none of the
  daemon's `NARSOR_*` variables; the enrollment code reaches it only as
  `NARSOR_ENROLLMENT_CODE`, and never reaches a runner;
* its log lines are in the daemon's log, prefixed with `narsor-controller:`;
  `RUST_LOG` follows the daemon's log level;
* a controller that dies is started again (1 s, doubled up to 30 s) with the
  same identity; running tasks keep their runners;
* on stop the daemon sends the controller `SIGTERM`, waits up to 10 s for the
  lease release, then sends `SIGKILL`, and only then closes the API; a
  controller whose daemon was killed stops on its own;
* one controller per credential: it holds `<credential>.lock` beside the
  credential and waits up to 7 s for a predecessor.

`narsor-server`, `narsor-controller` and `narsor-runner` must be installed in
one directory; the daemon looks for the controller only next to itself, never
on `PATH`. In network mode the daemon starts no local controller. Remote tasks
on any backend use the same owner API.

## 2. Server modes: trusted and network

The mode is a setting, never inferred (`narsor-cli config show` lists it). A
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
narsor-cli auth claim <code>
narsor-cli auth whoami
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
  # optional slot-release periods, in whole seconds (1 to 86400); defaults shown
  slot_release_grace_secs: 300           # wait before a task in a transient state frees its slot
  slot_release_read_extension_secs: 180  # what a read of the task's details adds to that wait
  slot_reader_idle_secs: 120             # idle time after which the read-only runner of a stopped task is removed
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
# {catalog}/repositories/api.yaml (narsor-cli repo add …, then edit)
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
`environment_identity`. The harness recorded there is the task's fallback, not
a pin: a running task follows the installation's default harness (see
"Scheduling").

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
| `claude_profile_secret` | `profileSecret` | Optional dedicated Claude profile Secret; mounted read-only into every Claude Code runner |
| `claude_profile_files` | — | Optional list of `secret-key=relative/path` mappings for settings, auth, MCP, plugins and skills files (default: the key `settings.json` at `settings.json`) |

Other `backend.yaml` keys: `namespace`, `storage_mib` (PVC size, default
10240), `run_as_user` / `run_as_group` / `fs_group` (default 1000),
`start_timeout_secs` (default 900: how long a Pod may take to report ready
before it counts as lost), `harness_home` (below).

When `claude_profile_secret` is set, every Claude Code runner Pod mounts the
configured files from that dedicated Secret read-only. For example:

```yaml
claude_profile_secret: narsor-claude-profile
claude_profile_files:
  - settings=settings.json
  - auth=.credentials.json
  - mcp=.mcp.json
  - review-skill=skills/review/SKILL.md
```

Each left-hand value is a Kubernetes Secret key; each right-hand value is a
relative path in Claude's configuration directory. Include the
authentication, settings, MCP, plugin and skill files Claude Code needs in
this Secret; do not include task session history.

The mount is the profile's *source*; the task's private
`$HOME/.claude` on the PVC is its *work directory*, exported as
`CLAUDE_CONFIG_DIR`. Before every start of Claude Code the runner merges the
source into the work directory, file by file, against the copy of the
source it saw at the previous start (kept in `narsor/claude-profile/` on the
PVC):

* a file the harness did not change takes the Secret's version, so a new
  OAuth login reaches resumed tasks;
* a file the Secret did not change keeps the harness's version;
* a file changed on both sides: `.credentials.json` is taken from the
  Secret whole; other JSON files (`settings.json`, `.claude.json`, plugin
  registries) are merged key by key, lists such as permission rules are
  united, and a value changed on both sides is the Secret's; any other file
  is the Secret's;
* files the Secret never had (sessions, history, todos) are left alone.

The volume's own entries (`..data`, `..<timestamp>`) are not part of the
profile. Profile files are written readable by their owner only. Nothing is
ever written back: changes a task's harness makes stay in that task.
`harness_home.claude_profile_source` is set from `claude_profile_secret` and
may not be set by hand. Set the same name in the admission ConfigMap's
`profileSecret` and the controller Role's `resourceNames` rule.

**Editing the profile.** The controller is the only part of Narsor that
writes the profile Secret; its Role may `get` and `update` that one Secret
by name, and task Pods have no Kubernetes access at all. To change the
profile — log Claude Code in again, add permission rules, MCP servers,
plugins or skills — run an edit session in the controller's Pod:

```bash
kubectl -n narsor-user exec -it deploy/narsor-controller -- \
  /opt/narsor/bin/narsor-controller kubernetes edit-claude-profile
```

The controller image must have Claude Code on its `PATH` (or name it with
`--claude-command`). The session:

1. reads the Secret and writes each mapped key at its path in a private
   directory under `NARSOR_PROFILE_EDIT_DIR` (`/tmp`, an `emptyDir`);
2. runs Claude Code in your terminal with that directory as
   `CLAUDE_CONFIG_DIR`, an empty private `HOME` and no API key; make your
   changes and close it;
3. collects the profile files — the mapped paths, plus `settings.json`,
   `settings.local.json`, `.credentials.json`, `.claude.json`, `.mcp.json`,
   `CLAUDE.md`, `skills/`, `agents/`, `commands/`, `output-styles/` and the
   plugin registries; never `projects/`, history, todos, shell snapshots or
   plugin caches;
4. shows a line diff of every changed key, with control characters escaped
   and the login's content hidden, warns about every change but the login
   and a few harmless settings (`model`, `theme`, `outputStyle`, `language`
   and the like) — permissions, hooks, MCP servers, plugins, skills, agents,
   commands, `CLAUDE.md`, mapped scripts — then asks before saving and writes
   the changed keys, keeping every other key of the Secret. `--yes` skips
   the question only when there is no warning, e.g. to rotate the login.

A profile file with no mapping is reported as not saved: add a
`key=path` entry to `claude_profile_files`, restart the controller and edit
again. A mapped file you delete stays in the Secret, because task Pods mount
every mapped key. The write carries no `resourceVersion`: when the Secret was
changed outside Narsor meanwhile, the last write wins. Changes made outside
Narsor are accepted as they are. Task Pods take the new profile at their
next start, through the merge above. The session directory, login included,
is removed when the command ends.

**`harness_home`** — which `HOME` and harness configuration the runner gives
every action, passed as `narsor-runner` flags on every Pod start:

```yaml
harness_home:
  mode: private          # default: a private HOME on the task PVC; or: host
  codex_home: /mnt/codex # optional, exported as CODEX_HOME
  claude_config_dir: /mnt/claude  # optional, exported as CLAUDE_CONFIG_DIR
```

`host` is the image's own `HOME`, which is not on the task PVC: a new Pod
starts without the harness sessions of the previous one. A harness directory
must exist in the image or on a mount. Inherited `ANTHROPIC_API_KEY`,
`OPENAI_API_KEY` and `CODEX_API_KEY` never reach a harness unless the task is
given them as `env` secrets. For Claude Code on a subscription, a token from
`claude setup-token` delivered as `!env CLAUDE_CODE_OAUTH_TOKEN` needs no
writable login directory.

The Deployment runs `narsor-controller kubernetes`; every argument comes
from the environment (`narsor-controller kubernetes --help` lists the
variables). `narsor-controller` without a subcommand is a usage error.

Set in `controller.yaml`: the Narsor release image **by digest**,
`NARSOR_CONTROL_PLANE` (the URL the controller uses) and, when task Pods
reach the control plane under another URL, `NARSOR_RUNNER_CONTROL_PLANE`.
Task Pods need network egress to that URL.

## 6. Enrolling a controller

Each controller has its own identity scoped to one backend.

1. Register the backend in the catalogue (`{catalog}/backends/k8s.yaml`).
2. As the owner, create a one-time code (shown once, expires, default 15 min):

   ```bash
   narsor-cli controller enroll-code k8s --name prod-cluster
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
   The code reaches the controller only as `NARSOR_ENROLLMENT_CODE` or in a
   file named by `--enrollment-code-file`; `--enrollment-code <code>` is
   refused, because every user of the machine can read a process's
   arguments.
5. Delete the enrollment Secret, then check:

   ```bash
   kubectl -n narsor-user delete secret narsor-controller-enrollment
   narsor-cli controller list        # the identity holds the backend's lease
   ```

Every controller request is signed (method, path, query, body, timestamp,
nonce); the control plane and the controller clocks must agree within
**±60 seconds**.

## 7. Controller credentials: rotation and compromise

* **Rotation.** The controller can replace its key
  (`POST /controller/v1/key`, signed with the old key and proven by the new
  one); the old key stops working at once. Re-enrolling with a new code and
  revoking the old identity is equally valid.
* **Revocation.** `narsor-cli controller revoke <controller-id>` ends the
  identity and fences its lease immediately; queued tasks of that backend
  show `controller offline` until another controller holds the lease.

**If a controller is compromised:**

1. Revoke its identity: `narsor-cli controller revoke <id>`.
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
a task under `/narsor`: at least `/narsor/bin/narsor-runner`, plus the supported
harnesses, their portable dependencies and skills. The init container runs
`cp -a /narsor/. /opt/narsor/`, so the image needs `cp` and the payload must be
self-contained (statically linked or carrying its own loader and libraries).
Tool images are copied the same way under `/opt/narsor-tools`.

### Repository base image

The main container runs the repository's own image with its command replaced
by `/opt/narsor/bin/narsor-runner`. The runner checks the contract before any
work and reports a readable `base_image_contract` problem otherwise:

* supported platform and architecture, `/bin/sh` and basic tools;
* `git` and a CA bundle (`/etc/ssl/certs/ca-certificates.crt` or equivalent);
* runs as **non-root** — by default uid/gid 1000 (`run_as_user`,
  `run_as_group`, `fs_group`);
* a writable mounted `$HOME` (the runner sets a private HOME on task storage
  unless `harness_home.mode` is `host`);
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
(runner state; the owner's `state` is computed from it and the execution, §10)
Queued ─▶ Provisioning ─▶ Running ─▶ PauseRequested ─▶ Stopped ─▶ (resume) Provisioning
                             │
                             └─▶ Interrupted / Unknown ─▶ (resume or start) Provisioning
                                   Unknown ─▶ (loss confirmed) Interrupted ─▶ (automatic resume) Provisioning
any state ─▶ Archiving ─▶ Archived
```

* **Pause** (the owner's, or the slot-release policy's, §10) asks the runner for a safe point; only after it acknowledges does
  the controller delete the Pod. The PVC — workspace, engine state, private
  `$HOME`, the harness the runner last switched to — stays.
* **Resume** creates a new Pod on the same PVC; nothing is cloned again.
* **Interrupted / Unknown**: the workload disappeared (a Pod that failed or
  was deleted, or Pod phase `Unknown`). Neither Kubernetes nor the controller
  restarts the Pod; the control plane **resumes an `Interrupted` task on its
  own** (D58): a new Pod on the same PVC, up to 5 consecutive attempts after
  0 s, 30 s, 2 min, 10 min and 30 min. A loss counts as consecutive when the
  runner was ready for less than 10 minutes (or never, e.g. a refused
  launch); a loss found after the controller restarted (a newer lease epoch)
  uses up no attempt — at most 5 of those in a row, after which a controller
  that keeps restarting has its losses counted like any other. Each resume leaves an `AutoResumed` event; when the
  attempts run out the task stays `Interrupted` with "could not resume
  automatically: …" and an `AutoResumeExhausted` event. Still waiting for the
  owner (`resume`, `start` or `archive`): a task lost while pausing, a task
  whose attempts are exhausted (an owner `resume`/`start` resets the count),
  and an `Unknown` task — it turns `Interrupted` only once the controller
  confirms the Pod is gone or terminated while the PVC is present; a Pod in
  phase `Unknown`, one still being deleted or a missing PVC keep it
  `Unknown`. Resumes need the backend's controller online.
  On the `local` backend a runner is marked with `NARSOR_LOCAL_WORKLOAD_UID`;
  before a resume the backend kills what is left of a dead runner's process
  group, but only while a live member of the group still carries that mark.
* **Upgrading to automatic resume**: upgrade the control plane (the daemon)
  first, then the controllers. An older control plane refuses a newer
  controller's `LossConfirmed`; the controller says so once per task in its
  log and the task stays `Unknown` until its owner acts. An older controller
  never sends it, so its `Unknown` tasks wait for the owner as before.
  Clients (CLI, TUI) built before this version cannot decode the new events
  (`AutoResumed`, `AutoResumeExhausted`) and observations (`LossConfirmed`)
  in the history and the event stream, so upgrade them together with the
  server; clients of this version and later read anything newer as
  "unrecognised" and keep going.
* **Archive** stops everything and deletes the Pod **and the PVC**. It wins
  over every other state and cannot be undone; there is no automatic
  retention cleanup.
* **Reading a stopped task (D63).** While a task is stopped (paused by the
  owner or freed by the slot policy) or interrupted, its conversation,
  events and conductor are read by a **read-only runner** (`narsor-runner
  --read-only`), on every backend. The control plane does not read the task's
  storage itself. Opening the task's details leases a reader on the task's
  record (`reader_lease`). The controller sees `reader_needed` in the
  desired state of the task and starts the reader as a workload of its own:
  on Kubernetes a second Pod `narsor-<key>-<hash>-reader` with the task's
  PVC mounted `readOnly`; on `local` a second process. The reader takes the
  storage lock like a working runner, writes nothing to the storage, and
  has its own credential (role `reader`, token prefix `nsrr.`, file
  `reader-session.json`) that the control plane refuses for every write. It
  takes no slot. Each read moves the lease later. After
  `slot_reader_idle_secs` without a read the control plane ends the lease and
  the controller removes the reader. Until the reader connects, a read
  answers `reader_starting` (a conversation page: `409
  conversation_reader_starting`). The TUI then shows "wczytywanie…" and asks
  again every 2 seconds. Any other reason (for example an archived task, or
  `Unknown`, whose runner may still hold the storage) is shown with the last
  snapshot and is not asked again in a loop.
* **Reader and work never run together.** Before the controller starts,
  provisions or archives a task, it removes the reader and waits for
  the confirmation. The command that resumes or archives the task ends the
  lease in the same record update. The storage lock is the second guard: a
  reader that finds the storage busy stops and reports it. If a reader cannot
  start (for example the admission policy refuses the Pod), the reason is in
  the **controller's log** only. The control plane answers `reader_starting`
  for as long as the lease lasts.
* Observations that are no longer current are shown as stale with their age,
  never turned into another state.

## 10. Scheduling

The slot-release periods (`slot_release_grace_secs`,
`slot_release_read_extension_secs`, `slot_reader_idle_secs`) are settings of
`limits`. Omit them to get 300 s, 180 s and 120 s. The third one,
`slot_reader_idle_secs`, is how long the read-only runner of a stopped task
(section 9) may idle before the control plane ends its lease and the
controller removes it. Each must be a whole number
from 1 to 86400; a negative or non-numeric value makes the backend file
unreadable, and 0 or more than 86400 makes the backend unusable with a clear
message. The control plane reads them again at every decision, so a change
applies without a restart.

* `max_concurrent_tasks` in the daemon configuration is one global task
  budget shared by every backend (default `1`; `none` is unlimited). A slot
  is held by work, not by a live process alone. Slots are held by tasks in
  `Provisioning`, `Running`, `PauseRequested`, `Interrupted`, `Unknown`,
  `Archiving`, and by an admitted `Queued` task. `Stopped` and `Archived` hold
  none. An interrupted task still blocks a slot until it is resumed
  (automatically or by the owner) or archived. The task list shows whether a
  task holds a slot (`holds_slot`), and the `slots N/M` counter counts only
  those that do.
* **Who frees a slot.** Only the control plane decides, from the execution
  state in the runner's last snapshot. A task that **completed or was
  cancelled** frees its slot at once. A task that waits for its owner (**input
  needed**) or **failed** frees it after the wait `slot_release_grace_secs`;
  opening the task's details moves that deadline later by
  `slot_release_read_extension_secs`. The control plane does not free a slot
  while an answer or an operator command of the owner waits for the runner.
  Freeing a slot is a pause issued by the policy (idempotency key
  `slot-release:<generation>`): the runner stops at a safe point, the
  controller removes the Pod or process and keeps the storage, and the freed
  slot goes to the next task in the queue. It is not a lost workload, so it
  never starts an automatic resume (D58). The deadline is kept in the task's
  record, so a restart of the daemon neither loses nor shortens it. The
  history shows `SlotReleaseScheduled` and `SlotReleased` (`terminal` or
  `grace_elapsed`).
* **What the owner sees.** The control plane computes one `state` for each
  task: `Queued`, `Running`, `HumanNeeded`, `Paused`, `Finished`, `Error` or
  `Archived`. A task stopped to free its slot keeps the state of its work
  (for example `Finished`); `Paused` means only the owner's own pause. The
  runner and its resources are a separate field, `runner_state` (older
  versions called it `lifecycle`, and `Stopped` was `Paused`; both old names
  are still read).
* Every task has a position (`rank`) in its backend's queue, shown as `#n` by
  `narsor-cli task list`. A new task joins the end; waiting tasks are admitted in
  that order. A queued task is reordered with `narsor-cli task move <task>
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
* The runner follows the installation's **default agent harness**
  (`narsor-cli harness default`, `h` in the TUI), not only the one the task was
  created with: every poll and snapshot answer of the control plane carries
  the default as it applies to the task (`default_harness`), and setting or
  resetting it wakes open long-polls. The harness in the task's
  `ResolvedEnvironment` (`--harness`, else the profile's) is only the
  fallback until the runner has heard a default; the runner keeps the
  harness it last switched to in `narsor/harness.json` on the task storage,
  so a resumed task starts on it. A switch takes effect at a clean boundary:
  an agent action in progress keeps its harness through its retries,
  reprompts and a resumed session, and the next agent action and the next
  conductor invocation take the new one. A `harness:` the workflow names
  itself always wins. When the task's backend does not run the default
  (the catalogue's `harnesses` allow list) or its binary is not on the
  runner's `PATH`, the runner stays on its current harness and reports
  `default_harness_unavailable` (harness, kept, reason) in its snapshot —
  information for the owner, shown in the TUI's details, not a problem and
  not a failure of a step.
* The runner hosts the task's **conductor** when the task's harness can run
  in its environment: the harness binary (`claude`, `codex`, `opencode`) must
  be on the runner's `PATH`, which on Kubernetes means the base image or a
  tool package brings it — the adapter configures nothing for it. The
  conductor runs on the runner's current harness — the installation's
  default as above, from its next invocation on; there is no per-task
  choice. A `SetConductorHarness` operator command is refused by the
  control plane with `400 unsupported_command`, and one queued by an older
  version is settled as rejected by the runner, changing nothing. It
  summarizes `human` gates, rescues a step's missing output label,
  diagnoses a failed step and answers the chat, always read-only. Without a
  usable harness there is no conductor: the runner reports
  `conductor_available = false` with the reason (`narsor-cli task conductor
  history`), a
  failed step waits for the owner as it is, without a diagnosis or
  proposals, and the conductor commands are refused with that reason. The
  conductor's state — chat, proposals, decisions, the diagnosis — is read
  through the control plane from the live runner
  (`GET /control/v1/tasks/:id/conductor`), or, for a stopped task, from its
  read-only runner (D63). A runner restarted on its storage
  answers the chat messages it left unanswered and diagnoses again a failed
  step it had not concluded about.
* A queued task shows why it waits: `waiting for capacity` (position,
  slots in use / limit), `controller offline` (no controller holds the lease),
  `backend unknown`, or `admitted` (starting).
* Resuming a stopped task needs a free slot; on a full backend the task goes to
  the front of the queue (state `queued`, no reservation) and starts on the
  first slot released. The command response says `queued_for_slot`. The
  error `backend_at_capacity` no longer applies to a resume.
* A command that needs the runner to work (an answer to an input, an operator
  command, a message to the conductor) sent to a stopped task is stored and
  resumes the task (idempotency key `work-resume:<key of the command>`), so
  the owner does not resume it first. It runs through the queue like any
  resume, and never without a slot. The keys `slot-release:` and
  `work-resume:` (like `auto-resume:`) are reserved: an owner key that starts
  with one is refused with `400 reserved_idempotency_key`.
* Reading the details of a stopped task does not need a slot: a read-only
  runner reads it (section 9, D63) on every backend. Freed slots do not
  change that.
* **Upgrading to the read-only runner (D63)**: upgrade in this order. First
  the control plane (the daemon), then the controllers, then the clients
  (CLI, TUI) together with their server. Apply the changed
  `deploy/kubernetes/admission-policy.yaml` **together with the controller**
  (before or at the same time): it lets a Pod named `...-reader` mount only
  its own task PVC, and only `readOnly`. A new controller without the new
  policy cannot create the reader Pod, and the reads of stopped tasks stay at
  `reader_starting`. An older controller does not start readers. An older
  client does not know `reader_starting` and shows an ordinary
  "unavailable". Two points are not tested on a real cluster: `flock` on a
  read-only PVC (NFS and other real volumes), and the CEL expressions of the
  changed policy.
* The default backend applies to tasks created afterwards only; existing
  tasks never move.

## 11. Using it: CLI and TUI

Without `--server` the commands use the trusted local daemon (started when
none runs). With `--server` (or `NARSOR_SERVER`) they use a remote control
plane and a token (section 2).

```bash
narsor-cli backend default                       # get; also: set <backend>, reset
narsor-cli harness default                       # get; also: set <harness>, reset — new and running tasks
narsor-cli task create implement-feature --input ticket=NAR-1 \
  [--backend k8s] [--profile default] [--harness codex] \
  [--repository api --commit <sha>] [--workflow-version 1] [--task-id my-id]
narsor-cli task create --file ./workflow.yaml --input name=value   # local daemon only
narsor-cli task list [--archived] [--json]       # the attention mark `?`/`!`, the title; --archived: the history
narsor-cli task show <task> [--json]             # title, links, what it waits on, observations
narsor-cli task pause|resume|start|archive <task> [--expected-generation N]
narsor-cli task move <task> --before <other> | --after <other> | --top | --bottom
narsor-cli task input <task> --field reason=ok [--data k=v] [--variable k=v] \
  [--label approved] [--comment …] [--waiting-id <id>]
narsor-cli task interrupt <task> [--wait-secs N]
narsor-cli task transition <task> <state> [--override --reason <why>] [--wait-secs N]
narsor-cli task restart-step <task> [--note <text>] [--failure-key K] [--wait-secs N]
narsor-cli task conductor chat <task> <text> | history <task> [--after N]
narsor-cli task conductor confirm|reject <task> <proposal> | undo <task> <decision>
narsor-cli task conversation <task> [<attempt>] [--from N]
narsor-cli task events <task> [--follow]
narsor-cli task watch [--cursor C]               # every task, reconnecting from the cursor
narsor-cli task workflows                        # the catalogue: source, description, inputs
narsor-cli task validate <path>                  # every violation of a workflow file (local daemon only)
narsor-cli task capabilities                     # what this server offers
narsor-cli task repo list | add <id> --remote <url> | remove <id>
narsor-cli task history list | show|events|steps|conductor <execution> \
  | conversation <execution> <attempt> | recover <execution> [--comment …]   # local daemon only
narsor-cli task materials upload <task> <path>... [--id ID] [--label T] [--ttl-secs N]
narsor-cli task materials list|show <task> [<material>]
narsor-cli task materials download <task> <material> <dest-dir> [--force]
narsor-cli task materials delete <task> <material>
narsor-cli auth whoami | claim <code>
narsor-cli controller enroll-code <backend> | list | revoke <id>
narsor                                   # the interface; over --server when given
```

`narsor` is one interface for every backend: it talks to the control plane
only (`/control/v1`) — the local daemon without `--server`, the remote one with
it — and the backend is whatever the configuration names. It opens even when
the server cannot be reached and keeps trying; without a connection the data is
greyed with its age and actions that change state are blocked with the reason.
Every action of
the interface is a `narsor-cli task …` command: `move` (`K` / `J` / `m`), `pause` /
`resume` (`p`), `interrupt` (`x`), `transition` (`t`, with `--override` for a
reasoned override), `start` (`s`), `archive` (`a`), `input` and
`restart-step` (the answer panel), `conductor …` (the *Dyrygent* tab), `create`
(`n`, also `create --file` for a path, with `workflows` and `validate` for the
catalogue and the check), `list --archived` and `history …` (`H`), `repo …`
(`R`) and `capabilities`; besides those, `h` is `narsor-cli harness default set`
/ `reset`, the installation's default harness the header names. `?` shows the keys, [the TUI guide](../user/tui.md) describes the screens.

### Task materials

A *material* is a one-off, bounded set of regular files attached to a task:
input you hand the agent (a few files, a snapshot of a local folder) and
small documents the task leaves for you. It is a copy, never a live mount of
your machine, and it never becomes a commit in the product repository.

* **Upload.** `narsor-cli task materials upload <task> <path>...` snapshots the
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
  `narsor-cli task materials download <task> <material> <dest-dir>`: it writes
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
   narsor-cli task create <workflow> --backend <backend> --task-id smoke-1
   narsor-cli task show smoke-1          # Running; one Pod and one PVC labelled for the task
   narsor-cli task pause smoke-1         # Paused; the Pod is gone, the PVC remains
   narsor-cli task resume smoke-1        # Running again on the same PVC
   narsor-cli task archive smoke-1       # Archived; Pod and PVC deleted
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
* No automatic retention cleanup — an owner decision by design. Automatic
  resume after workload loss is bounded (section 9) and not configurable per
  task, workflow or backend.
* OIDC device-code login is not built in: use a token helper that writes a
  file or prints a token.
* Old executions (from before the scheduler) are read, and a failed one
  recovered, from the TUI's history (`H`) or with `narsor-cli task history …`,
  over the same control plane and only on the local daemon; the local
  `/executions` routes remain for `narsor-cli list`, `show` and the like.

## 14. A computer as a backend

A network control plane can run tasks on a computer — a laptop, a
workstation — with `narsor-controller local` started there by hand. The
controller only opens outbound connections, so the computer needs no open
port. **Read this first:** the runner runs every task as the computer's user,
without a container. Whoever controls the server can then run any program on
that computer and read every file its user can read. Use it only with a
server you control, and keep the computer's user free of what tasks must not
see.

For a dedicated Linux VM, the Release installer can write the network-mode
server configuration and workstation backend entry. It prints separate
commands for `narsor-server` and the controller, but installs no systemd unit
or Kubernetes resources. See [Linux installation and server
operation](installation.md) for role selection and the generated setup.

1. Register the backend under its own id, never `local` (a network server
   refuses `backends/local.yaml` at start; that id belongs to a trusted
   daemon). The trust scope must be `workstation`:

   ```yaml
   # {catalog}/backends/laptop.yaml
   kind: local
   trust_scope: workstation
   limits:
     max_concurrent_tasks: 1
     default_resources: { cpu_millicores: 1000, memory_mib: 2048 }
   ```

2. As the owner, create a code. The server refuses it without the explicit
   consent (`400 workstation_risk_not_accepted`); the CLI prints the risk:

   ```bash
   narsor-cli controller enroll-code laptop --accept-workstation-risk
   ```

3. On the computer, with `narsor-controller` and `narsor-runner` in one
   directory, start the controller. The code goes in the environment or in a
   file, never on the command line:

   ```bash
   NARSOR_ENROLLMENT_CODE=<code> narsor-controller local \
     --control-plane https://narsor.example \
     --storage ~/narsor-laptop
   ```

   The credential is kept `0600` in
   `$XDG_STATE_HOME/narsor/workstation-controller/credential` (or
   `--credential-file`). At start the controller prints the server's address,
   the backend it serves and a warning.

**Transport.** Beyond loopback (a loopback IP address or exactly
`localhost`; no DNS) the controller accepts only `https://`, checks the
server's certificate against the system's certificate store and follows no
redirect. There is no option to turn the check off: to use a private CA, add
it to the system store. `--runner-control-plane` follows the same rule, and
a runner follows no redirect either.
(Debug builds accept plain `http://` beyond loopback, for tests only.)

**What the server may ask for.** A control plane beyond loopback is not
trusted, so the controller checks every launch spec before anything of the
task is created:

* tasks get a private HOME on task storage. `--home host` (or `local_home:
  host` in the configuration, or `NARSOR_LOCAL_HOME=host`) needs
  `--accept-host-home`: tasks would then see your SSH keys, tokens and
  harness logins. `--codex-home`, `--claude-config-dir` and
  `--claude-profile-source` point at your own harness logins, so they need
  `--accept-host-home` too; without it the configuration file's
  `local_codex_home`, `local_claude_config_dir` and
  `local_claude_profile_source` are not used;
* with a private HOME the runner and everything it starts inherit none of
  your logins from the controller's environment: no `CODEX_HOME`,
  `CLAUDE_CONFIG_DIR`, `CLAUDE_CODE_OAUTH_TOKEN`, `ANTHROPIC_AUTH_TOKEN`,
  `GH_TOKEN`, `GITHUB_TOKEN`, `GH_ENTERPRISE_TOKEN`,
  `GITHUB_ENTERPRISE_TOKEN`, `GH_CONFIG_DIR`, `GITLAB_TOKEN`, `GL_TOKEN`,
  `GLAB_TOKEN`, `GLAB_CONFIG_DIR`, `SSH_AUTH_SOCK`, `SSH_ASKPASS`, `GIT_ASKPASS`,
  `GIT_SSH`, `GIT_SSH_COMMAND`, `XDG_CONFIG_HOME` and no `GIT_CONFIG*`
  variable;
* a local workspace directory must lie inside `--storage`;
* a Git workspace is cloned over `https://` only — never a path, a
  `file://`, `ssh://` or `git@host:` remote, nor a host on this computer or
  a private network (`localhost`, a loopback, private, link-local or
  carrier-grade NAT address; a name is not resolved). With a private HOME
  the clone also uses none of your own git setup: no `~/.gitconfig` or
  system configuration, no credential helper, SSH key or agent, no askpass
  helper. Only the credential the repository is pinned to is used, and git
  follows no HTTP redirect, so a public host cannot send the clone on to a
  private network;
* a secret goes only into a variable a supported harness or tool reads
  (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `CODEX_API_KEY`,
  `CLAUDE_CODE_OAUTH_TOKEN`, `GH_TOKEN`, `GITHUB_TOKEN`); allow another one
  with `--allow-env NAME`. A secret file stays inside the private HOME;
* the runner program is always the `narsor-runner` beside the controller.

These checks keep the server from choosing what the runner touches. They do
not keep a task's own code — what its harness runs — from reading the user's
files.

**Configuration.** Started by hand, the controller reads the Narsor
configuration file (`--config`, else the usual search) for the HOME settings
only: a flag wins over a `NARSOR_LOCAL_*` variable, which wins over the file.
`--no-config` reads no file.

**Losing the computer.** Revoke its identity at once:
`narsor-cli controller list`, then `narsor-cli controller revoke <id>`. Its
tasks show `controller offline` until another controller holds the backend.

**Manual test.** Against a network server with backend `laptop`: start the
controller as above, check `narsor-cli controller list` (it holds the lease),
create a task with `--backend laptop` and watch it run in the TUI next to the
tasks of the other backends.

Before this mode is released, a security specialist reviews the checks above
(R16, R17).
