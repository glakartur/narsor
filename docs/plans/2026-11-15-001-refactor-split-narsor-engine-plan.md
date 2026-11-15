---
title: "refactor: Split narsor-engine along its layers"
type: refactor
status: completed
date: 2026-11-15
origin: /simplify review of crates/narsor-engine (reuse, simplification, efficiency, altitude)
---

# refactor: Split narsor-engine along its layers

## Summary

`narsor-engine` (about 78k lines, 18 top-level modules) holds three layers
in one compilation unit:

1. **Model** — `domain`, `workflow`, `config`.
2. **Runner-side execution** — `engine` and `conductor` (mutually dependent,
   must stay together), `workspace`, `tasks`, `workflows`.
3. **Server-side control plane** — `orchestration`, `observers`,
   `materials`, `auth`, `registry`, `environment`, and the server half of
   `controller_protocol` (`service/`, about 10k lines).

Every binary links all of it. `narsor-runner` uses only `engine`,
`conductor`, `workflow`, `config` and `domain`. `narsor-controller` uses only
`config`, `backends`, `domain`, `controller_runtime`, `controller_protocol`
(client side), `registry`, `environment` and `auth`. So about 28k lines of
server-only code, and server-only dependencies (`jsonwebtoken`,
`ed25519-dalek`, `zstd`, OpenSSL via `github-copilot-sdk` defaults), are
compiled into the runner and controller. Any edit in `orchestration` rebuilds
the engine and relinks all three binaries.

The goal is a set of crates with downward-only dependencies, reached in
small steps that each build and pass `cargo test --workspace` on their own.

---

## Target layering

| Layer | Crate | Content | Consumers |
|---|---|---|---|
| L0 | `narsor-model` | `domain`, `config`, `workflow` (schema, loader, template, validator), pure name rules | everyone |
| L1 | `narsor-store` | persistence primitives: `atomic`, `lock`, `paths`, `ExecutionStore`, `ConversationStore`, `step_profile` I/O — without `recovery` | L2+ |
| L2a | `narsor-exec` | `engine` + `conductor`, `workspace`, `tasks`, `workflows`, `api` projection, recovery | runner, server |
| L2b | `narsor-control-plane` | `orchestration`, `materials`, `observers`, `auth`, `registry`, `environment`, `launch`, `controller_protocol` (`service`, `credentials`, `error`) | server |
| L2c | `narsor-controller-client` | `controller_client` (`client`, `wire`, `pop`), `controller_runtime` adapter | controller, runner, server |

L2b may depend on L2a at first: `controller_protocol::service` uses
`engine::{plan_operator_transition, manual_override_allowed,
HumanInputDelivery}` and `conductor::ConductorView`. Long term, L2b should
see only L0 data types.

`narsor-engine` stays as a facade that re-exports the new crates until
`narsor-server` imports them directly.

---

## Steps

### Step 1 — Remove upward edges with small moves (done 2026-11-15)

- `FileLock` moved from `orchestration::store` to `persistence::lock`, with
  its own `FileLockError`. `materials` and `observers` no longer import
  `orchestration` for locking.
- `tier_rank` and `effort_rank` moved to `config` (beside `MODEL_TIERS` and
  `EFFORT_SCALE`). `workflow::validator` no longer imports `engine`.
- `Setting` moved to `domain::profile`. `persistence::step_profile` no
  longer imports `engine`.
- `is_usable_directory_name`, `repository_name` and
  `FALLBACK_WORKSPACE_NAME` moved to `domain::workspace`. `workflow`,
  `registry`, `workspace::state` and `environment` no longer import
  `workspace::provisioner` or `tasks` for them. The old paths re-export them.
- Protocol types (`OwnerRef`, `EventWait`, `EventWatch`,
  `ConversationAttemptSummary`, `OperatorCommand`) are imported from
  `narsor_protocol::controller`, not through `crate::orchestration`.

Open from step 1: `ObserverName` (`observers/model.rs`) is still imported by
`workflow::validator`. Moving it needs `validate_name_like`,
`OBSERVER_ID_PREFIX` and an error type that does not depend on
`ObserverError`.

### Step 2 — Make `persistence` a pure storage layer (done 2026-11-15)

- Deleted the daemon-era startup recovery that had no production caller
  since 09c52c6: `recover`, `reconcile_all`, `compress_closed_conversations`,
  `sweep_orphaned_workspaces`, `RecoveryReport`, `OrphanedWorkspace`,
  `OrphanReason`. Their integration tests went with them; tests that used
  `recover` only to reconcile one execution now call `reconcile`.
- Moved the live part (`reconcile`, `Reconciliation`,
  `ReconciliationOutcome`) from `persistence::recovery` to
  `engine::recovery`, re-exported from `engine`. `persistence` no longer
  imports `conductor`, `engine` or `workspace`.

Behavior note: nothing now compresses the plain conversation file of an
attempt that a crash left open. `ConversationStore::close_attempt` still
compresses on a normal close.

### Step 3 — Break `domain` ↔ `workflow` (done 2026-11-15)

The only code edges from `domain` to `workflow` were two methods that call
the validator. Their inherent impls moved out of `domain`; the public API
is unchanged:

- `WorkflowDefinition::validate()` → `workflow/validator.rs`.
- `TaskLaunchSpec::definition()` (decode + check + validate the snapshot)
  → new `workflow/launch.rs`.

`domain` now references `workflow`, `orchestration` and `environment` only
in doc comments. `workflow` depends on `config`, `domain` and — through
`ObserverName` — `observers` (open item from step 1).

Constraint for step 8: inherent impls cannot cross crates, so `domain` and
`workflow` must land in the same L0 crate (`narsor-model`). If they ever
need separate crates, turn these two methods into free functions or an
extension trait in `workflow`.

### Step 4 — Move engine data types that `orchestration` reads (done 2026-11-15)

- `Timeline`, `ConversationState` and `EventLink` were `narsor_protocol::api`
  re-exports; `orchestration` now imports them from there.
- The human-input records moved from `engine/actions/human.rs` to new
  `domain/human_input.rs`: `PendingHumanInput`, `HumanInputDelivery`,
  `HumanInputRequest`, the permission request/decision types,
  `OutputProblem`, `RejectedAnswer`, `OutputRequirement`,
  `ConductorAnalysis`, `InputContinuation`, `ApprovalCandidate`,
  `AgentSessionProvenance`, `EntryLimitDetails` and
  `APPROVE_OUTPUT_LABEL`, with their constructors. The logic (`suspend`,
  `resume`, `resolve_permission`, permission rules) stays in the engine,
  and `engine::actions::human` re-exports the moved types, so no caller
  changed.

`orchestration` no longer imports `engine`. Its code depends on `domain`,
`environment`, `materials` and `persistence`; `controller_protocol` appears
only in a doc comment.

### Step 5 — Move `environment::resolver` to the control-plane level (done 2026-11-15)

`environment/resolver.rs` was split:

- `environment::resolver` keeps the contract: `LaunchIntent`,
  `ResolvedLaunch`, `LaunchError` and the `LaunchResolver` trait.
- The implementation moved to a new top-level module `crate::launch`:
  `CatalogLaunchResolver`, `HarnessDefaults`, `StaticHarnessDefaults`,
  `OwnerDefaults` and their helpers, with the precedence documentation.
  `narsor-server` and the integration tests import these from
  `narsor_engine::launch`.

`environment` code now depends only on `domain`, `persistence`
(`CatalogPaths`, `write_atomic`) and `registry` (`RegistryError` inside
`LaunchError`). `launch` carries the upward edges (`engine`, `workflow`,
`workflows`, `workspace`, `registry`) and belongs to L2b.

### Step 6 — Move controller-only and test-only code out (done 2026-11-15)

- `backends/local.rs` → `narsor-controller/src/local/backend.rs`
  (`narsor_controller::local::backend::{LocalBackend, LocalBackendConfig,
  LaunchCheck}`). `workspace::provisioner::USER_GIT_ACCESS_ENVS` became
  `pub` for it.
- `backends/fake.rs` → `tests/integration/backend_contract_support/fake.rs`.
  It could not go to `narsor-controller-runtime::testkit`: it drives a
  simulated runner through the engine's `RunnerClient` and `RunnerSnapshot`,
  and `narsor-controller-runtime` does not depend on the engine. Its only
  users are the backend contract tests.
- `narsor_engine::backends` is gone. It only re-exported the
  `narsor_controller_runtime::backend` contract; `narsor-controller` and the
  tests import that directly.

The engine library lost about 1.8k lines that the server and the runner
never used.

### Step 7 — Split `controller_protocol` (done 2026-11-15)

A new top-level module `crate::controller_client` holds the client side:

- `client.rs` (`ControllerClient`, `RunnerClient`, `ClientOptions`) and
  `wire.rs` moved from `controller_protocol`;
- `pop.rs` (proof-of-possession signing) moved from `auth`;
- `MaterialAckRequest` and `MaterialAckResponse` moved from `materials` into
  `wire`;
- `RunnerClient::upload_conductor_read` takes `&impl Serialize` instead of
  `&conductor::ConductorView`, so the client no longer depends on the
  conductor.

The server side re-exports these modules downward (`controller_protocol::
{client, wire}`, `auth::pop`, `materials::MaterialAck*`), so server paths
did not change. `narsor-runner` and `narsor-controller` import from
`narsor_engine::controller_client`; the runner no longer re-exports
`controller_protocol`.

`credentials.rs` and `error.rs` stay with the server: `CredentialStore`
verifies controller and runner requests, and `ProtocolError` maps
server-side errors. Plan correction: they belong to L2b, not L2c.

Follow-up (done 2026-11-15): `controller_client` no longer imports
`orchestration`. The simpler of the two options was taken — move the
conversions, not the types:

- `RunnerSnapshot` ↔ `RunnerSnapshotRequest` became
  `RunnerSnapshot::to_request` and `RunnerSnapshot::from_request` in
  `orchestration/model.rs`, beside the type, as did
  `From<MoveTarget> for MoveTaskTarget`.
- `RunnerClient::snapshot` takes `&RunnerSnapshotRequest`. The runner's
  `send_snapshot` helper encodes first and reports an encoding failure as
  `ClientError::Server`, as before.
- The owner views (`From<&QueuedInput> for InputView`,
  `From<&QueuedOperatorCommand> for OperatorCommandView`,
  `From<&QueueStatus> for QueueView`) moved to the server-side
  `controller_protocol/views.rs`.

`controller_client` now depends only on `domain` and `narsor-protocol`.

### Step 8 — Extract the crates

In this order: `narsor-model`, `narsor-store`, `narsor-exec`,
`narsor-control-plane`, `narsor-controller-client`. After each, update
`docs/dev/components.md` and the dependency diagrams.

#### 8a — `narsor-model` (done 2026-11-15)

- `domain/` and `workflow/` moved (`git mv`) to `crates/narsor-model/src/`.
  `config` there is `narsor_shared::config::*` plus `tier_rank` and
  `effort_rank`.
- `narsor-engine` re-exports `narsor_model::{domain, workflow}` and
  `narsor_model::config::*`, so `narsor_engine::domain::…` and every
  `crate::domain::…` inside the engine still resolve.
- Prerequisites done first:
  - the observer naming rule moved to `domain::observer_name`
    (`observer_name_problem`, `name_like_problem`, `MAX_OBSERVER_NAME_LEN`,
    `OBSERVER_ID_PREFIX`); `ObserverName::parse` and the workflow validator
    both use it, with unchanged messages. `workflow` no longer imports
    `observers`;
  - `persistence::paths` is a plain `pub mod` again, not mounted through
    `config` with `#[path]`.
- Fallout of the orphan rule: `From<&WorkflowExecution> for
  ExecutionSummary` moved from `engine::api::dto` to
  `domain/workflow_execution.rs`.
- `prompt_text::placeholder`, `PromptText::PART_SEPARATOR` and
  `template::render_scalar` became `pub` (the engine uses them).
- `docs/dev/components.md` and `AGENTS.md` list the new crate.

#### 8b — `narsor-store` (done 2026-11-15)

- `persistence/*.rs` moved to `crates/narsor-store/src/` (`mod.rs` became
  `lib.rs`); paths inside were rewritten (`crate::persistence::` →
  `crate::`, `crate::domain` → `narsor_model::domain`).
- `narsor-engine` re-exports it as `persistence`
  (`pub use narsor_store as persistence`), so no engine or server path
  changed.
- Became `pub` because the engine uses them: `atomic::write_and_sync`,
  `write_and_sync_with_mode`, `sync_parent_dir`, `lock::FileLock`
  (`acquire`) and `PersistenceError::io`.
- Removed `ConversationStore::open_attempt_ids`: its only caller was the
  compression sweep deleted in step 2.
- Dependencies: `narsor-model`, `narsor-protocol`, `base64`, `chrono`,
  `libc`, `serde`, `serde_json`, `thiserror`, `tokio` (`sync`, `rt`),
  `tracing`, `uuid`, `zstd`. The engine's manifest dropped `zstd`, which
  only persistence used.

#### 8c — `narsor-controller-client` (done 2026-11-15)

- `controller_client/{client,pop,wire}.rs` moved to
  `crates/narsor-controller-client/src/` (`mod.rs` became `lib.rs`).
- The engine's `controller_runtime` adapter (`impl ControllerLink for
  ControllerClient`, enrollment) moved with it as
  `narsor_controller_client::controller_runtime`: after the move both the
  trait and the type are foreign to the engine, so the orphan rule requires
  it.
- `narsor-engine` re-exports the crate as `controller_client` and the
  adapter as `controller_runtime`; server and runner paths did not change.
- `narsor-controller` depends on `narsor-controller-client` directly for
  the client and enrollment.
- Found while building the crate alone: `narsor-controller-runtime` uses
  `tokio::select!` without the `macros` feature and compiled only through
  workspace feature unification. Its manifest now lists `macros`.

#### 8d — `narsor-exec` (done 2026-11-15)

- `engine/`, `conductor/`, `workspace/`, `tasks/`, `workflows/`, `api/` and
  `registry/` moved to `crates/narsor-exec/src/`. `registry` joined the
  execution layer: `tasks` uses it, and it depends only on `domain`,
  `persistence` and `workspace`.
- The new crate's root re-exports `narsor_model::{config, domain,
  workflow}` and `narsor_store` as `persistence`, so every `crate::…` path
  inside the moved code still resolves; no source path was rewritten.
- `narsor-engine` re-exports the seven modules under their old names.
  `workspace::provisioner::check_ref` became `pub` (used by `launch`).
- Dependencies moved with the code: `github-copilot-sdk` and `base64` left
  the engine's manifest (only the execution layer uses them), as did
  `ed25519-dalek` and `rand` (unused since `pop` moved in 8c).

What is left in `narsor-engine` is the control plane (L2b): `auth`,
`controller_protocol`, `environment`, `launch`, `materials`, `observers`,
`orchestration`, plus the re-exports. Renaming it to `narsor-control-plane`
is optional.

#### 8e — the runner links only the execution layer (done 2026-11-15)

- `RunnerSnapshot`, `PendingInputRef` (with `to_request`/`from_request`)
  and `RETIRED_CONDUCTOR_HARNESS` moved from `orchestration/model.rs` to
  `narsor-model` (`domain/runner_snapshot.rs`); `orchestration` re-exports
  them.
- `materials` was split: the model, manifest rules, bundles, paths,
  snapshots and the runner-side writing (`mod.rs`, `bundle.rs`, `local.rs`,
  `path.rs`, `snapshot.rs`) moved to `narsor_exec::materials`; the server's
  `store.rs` (`MaterialStore`, `Staging`) and the `MaterialAck*`
  re-exports stay in `narsor_engine::materials`, which re-exports the rest.
  `narsor-exec` gained `sha2`.
- `narsor-runner` depends on `narsor-exec` and `narsor-controller-client`
  instead of `narsor-engine`. Its `orchestration` imports now name
  `narsor_protocol::controller` (most were re-exports) or
  `domain::runner_snapshot`.

`cargo tree -p narsor-runner` no longer contains `narsor-engine` or
`jsonwebtoken`.

---

## Follow-ups

The optional work left after step 8 — renaming `narsor-engine`, the
independent cleanups from the review and two security findings — is in
`docs/follow-ups/2026-11-15-narsor-engine-split-follow-ups.md`.
