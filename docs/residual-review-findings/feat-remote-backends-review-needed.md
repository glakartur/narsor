# Review needed — `feat/remote-backends`

Plan: `docs/plans/2026-10-01-2329-feat-zdalne-backendy-wykonawcze-plan.md`
Commits: `6fde9a3..278b0e0` (U1–U10 plus two security fixes), about 44k changed
lines in 130 files.

**No code review was run on this branch.** The owner chose to skip the Tier 2
`ce-code-review` and `/simplify` passes at the shipping gate. Each unit was
written by a subagent. The orchestrator checked its scope, re-ran its targeted
tests and committed it. That is not a review. The branch must not be exposed on
a network or merged before the areas below get a thorough review.

State at the time of writing:
- `cargo test` passes: 1136 tests, 0 failures.
- `cargo clippy --all-targets` reports 0 warnings and `cargo fmt --check` is clean.
- No live Kubernetes cluster and no real OIDC provider were used. Everything
  runs against in-process mocks.

Areas are ordered by risk. Each lists what was actually built and what a
reviewer should try to break.

## P0 — Authentication and ownership (`src/auth/`, ~2.3k lines)

- **OIDC validation (`oidc.rs`).**
  - Checks to verify: the asymmetric-only algorithm allow-list, that the JWK's
    `alg` and key type match the token's, exact issuer match, audience
    containment, 30 s leeway.
  - JWKS refresh is limited to one refetch per 60 s on an unknown `kid`. Check
    that this limit cannot be used to starve legitimate key rotation.
  - Tested only against an in-process mock issuer.
- **Single-owner claim (`owner.rs`).**
  - Claims must be atomic: one winner under concurrent claims. Claim files are
    0600 and the code is regenerated on every start.
  - Re-binding uses `owner_subject`. Check the issuer-change guard at startup.
- **Owner gate (`gate.rs`).**
  - Trusted mode must stay loopback-only. Check address resolution, IPv4-mapped
    addresses and the wildcard bind.
  - Network mode relies on a trusted proxy. The `X-Forwarded-Proto`
    every-hop-must-be-https rule was fixed in `c2a1f68`; re-check proxy CIDR
    handling.
  - Owner streams must end when the token expires or ownership is lost.
- **Controller proof-of-possession (`pop.rs`).**
  - Each request is signed with Ed25519 over the method, path and query,
    timestamp, nonce and body hash. Allowed clock skew is ±60 s.
  - Check nonce storage, which must be bounded and safe across restarts.
  - The body is buffered up to 1 MiB before verification. Check that a large
    body cannot be used for denial of service.
  - Check key rotation and that revocation also fences the controller's lease.
- **Discovery file (`src/api/discovery.rs`).** Clients refuse a non-0600 or
  foreign-owned `daemon.json`. Check the network-mode empty-token behaviour.
- **Known gap.** The legacy `/executions` routes have only the single-owner
  gate and no per-task owner.

## P0 — Credentials and secrets

- **Credential handling (`src/controller_protocol/credentials.rs`).**
  - Covers enrollment codes, runner `nsr.` credentials bound to task,
    generation and incarnation, SHA-256 digests compared in constant time, and
    0600 files.
- **Secret references (`src/domain/environment.rs`, `src/environment/`).**
  - Values must never be serialized. Today a sentinel-leak test covers DTOs,
    events, launch specs and files on disk. Look for paths it does not cover:
    logs and error messages.
- **Runner-side secrets (`src/runner/secrets.rs`).**
  - `env` delivery sets the variable on the runner process.
  - `file` delivery symlinks into the private HOME. Check that an agent in the
    workspace cannot redirect the symlink target.
- **Git clone credential (`src/workspace/provisioner.rs::clone_pinned_commit`).**
  - It is passed through a one-shot `-c credential.helper` that runs `cat` on
    the mounted file. Check quoting and injection through the file path, and
    that `.git/config` stays clean.
- **API client (`src/api/client.rs`, `src/api/token.rs`).**
  - Every request goes through `ApiClient::send`. A 401 triggers a token
    re-read, and `Debug` output is redacted.
  - `TokenSource::Command` runs a configured command. Check how that command
    is invoked.

## P0 — Kubernetes adapter and deployment (`src/backends/kubernetes/`, `deploy/kubernetes/`)

- **The admission CEL has never been evaluated by a real API server.** Only
  YAML structure and the Rust mirror `check_pod_shape` are tested. Run it on
  Kubernetes ≥ 1.30 with hostile Pod and PVC specs.
- **RBAC.**
  - The controller has pods get/create/delete, pvcs get/create/delete, secrets
    create only, events get/list, and a ClusterRole to get storageclasses.
  - Confirm it has no exec, attach, port-forward or ephemeral containers.
  - Confirm the plan's caveat holds: Pod creation is itself a secret-injection
    capability, so the admission policy is what limits it.
- **Pod shape.**
  - Expected: non-root, drop ALL capabilities, seccomp RuntimeDefault,
    `restartPolicy: Never`, no service-account token, and only approved Secret
    keys in a projected volume.
  - PATH is spelled out in full, so the base image's own PATH entries are lost.
- **Archive as destruction.**
  - Only a storage class with `reclaimPolicy: Delete` is allowed.
  - The adapter cannot see a per-PV reclaim policy, so an admin patching a PV
    to `Retain` defeats it.
  - Deletes use UID preconditions.
- **Observation mapping.**
  - Pod phase `Unknown` maps to the task being Unknown.
  - After `start_timeout_secs` a Pending Pod is treated as lost.
  - `resource_generation` is computed as `launch_generation × 8 + phase rank`.
  - Check monotonicity across lease handoff.
- **Release placeholder.** The controller Deployment's image digest is all
  zeros and must be set per release.
- **Disposable-namespace smoke cycle not run.** The full
  create/pause/resume/archive cycle on a disposable namespace (Verification
  Contract) is not automated and was never run.

## P1 — Distributed protocol correctness (`src/controller_protocol/`, `src/orchestration/`, `src/backends/controller.rs`)

- **Leases and fencing.**
  - Each backend has a lease epoch. Reports are fenced by epoch, desired
    generation and resource generation.
  - The controller checks its lease at the start of each step only. Backend
    writes in the window after another controller took over are fenced only by
    server-side checks and Pod labels.
- **Pause safe point.** The check now lives in `ControlPlane::report`.
  `exited_before_ack` in the controller settles a pause/exit race that showed
  up under slower signed requests. Review that race and its fix.
- **Commit protocol.**
  - The record is the commit point, with an outbox of one event; `recover()`
    repairs the event log.
  - Per-task `flock` serialization. `flock` may not work on network
    filesystems, and the task storage lock in the runner has the same
    limitation.
- **SSE.** It uses a composite per-task cursor. Re-reading the durable log on
  each notification should give exactly-once delivery. Check slow-consumer
  behaviour: a lagging consumer triggers a full rescan.
- **Runner recovery decision.**
  - After an unclean stop the runner recovers only when all of these hold:
    `authorized_by` is set, the incarnation is new, and the generation is newer.
  - Check that a missed or unexpected owner command cannot replay work.
- **Conversation relay.** Owner requests for conversation pages wait in memory
  for the runner, with a 20 s timeout. Check memory bounds under many
  concurrent requests.

## P1 — Scheduling (`src/orchestration/scheduler.rs`)

- **Admission.** Each backend has a schedule lock. Reservations are stored on
  the task record. `StartAccepted` is refused without a reservation.
- **Slot holding.** Interrupted, Unknown and Archiving tasks hold a slot. An
  ignored interrupted task blocks a slot indefinitely; this was deliberate.
  [Update, D62: the deliberate holding ended for tasks that completed, were
  cancelled, wait for input or failed; they free their slot (see
  `ARCHITECTURE_DECISIONS.md`). Only the cases listed below still hold it.]
  Since D58 an interrupted task is resumed automatically, so only a task whose
  automatic resume is exhausted, one lost while pausing, or an `Unknown` one
  holds its slot until the owner acts.
- **Periodic pass.** A pass runs every 5 s in the daemon
  (`spawn_periodic_admission`). Check its cost with many tasks.

## P1 — Materials (`src/materials/`, `src/runner/materials.rs`)

- **Bundle format and parsing.** The format is `NMB1` + manifest + blobs, and
  bounds are checked before any blob is read. Fuzz the parser.
- **Path rules (`path.rs`).** These guard local downloads (`local.rs`) against
  traversal and symlinks. This is exactly the risk the plan's deferred security
  note describes.
- **Runner outbox publishing.** Folders written by the agent are published
  automatically. Check that an agent cannot use it to send out files outside
  `$NARSOR_OUTBOX_DIR`.
- **Delete and expiry scope.** Owner delete and expiry remove only the control
  plane's copy. Copies already delivered stay on task storage until archive.

## P2 — Runner and environment (`src/runner/`, `src/environment/`, `src/domain/task.rs`)

- **Launch spec v3.** The workflow definition is pinned. Older versions fail to
  deserialize with a message asking to re-create the task.
- **Initialization.**
  - Storage that holds unexpected content is refused.
  - A partial clone is redone.
  - A complete clone that has no initialization marker is adopted. Check the
    adoption logic.
- **Preflight.** Preflight is Linux-only, x86_64/aarch64, non-root. The runner
  tests skip themselves when run as root.
- **Offline credential expiry.** A runner offline longer than the credential
  TTL (15 min) gets 401 until the controller re-mounts a credential.

## P2 — Clients (`src/cli/remote.rs`, `src/tui/remote*.rs`)

- **Large and lightly reviewed.** `tui/remote.rs` is ~1.5k lines and has been
  reviewed only through its state tests.
- **Gaps.** There is no OIDC device-code login, the remote TUI cannot create
  tasks, and there is no backend-list endpoint.

## Simplification

`/simplify` was not run. Expect duplication across the subagent-written
modules. Look first at:
- HTTP error mapping in `controller_protocol/http.rs` and `api/routes.rs`.
- Lock helpers in `orchestration/store.rs`, `runner/storage.rs` and
  `materials/store.rs`.
- The test fixtures in `tests/*_support/`, which repeat each other.
