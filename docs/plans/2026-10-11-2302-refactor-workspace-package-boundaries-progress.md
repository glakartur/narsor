# Workspace package boundaries — implementation progress

This note records implementation evidence without changing the implementation
plan. The plan remains the authority for scope and completion.

## Completed slices

- Established the Cargo workspace and protocol/server/runner/controller package
  scaffolding.
- Moved local REST request/response values, conversation page types, controller
  view values, and execution status wire values into `narsor-protocol`.
- Moved execution problem and conductor activity summary DTOs, including their
  wire enums, into `narsor-protocol`; server adapters now convert from internal
  domain enums.
- Moved conductor chat response entries and their author/kind enums into
  `narsor-protocol`; API clients and routes now exchange those wire types, with
  explicit conversions at the remaining internal TUI history boundary.
- Moved the conductor annotation response value into `narsor-protocol`; the
  server adapter converts its durable annotation into the protocol DTO.
- Moved the serialized execution timeline values (`Timeline`, `Position`,
  status, conversation, and event-link types) into `narsor-protocol`. The
  engine keeps the event-folding implementation and re-exports these types;
  the HTTP client now names the protocol timeline directly.
- Moved the serialized human-input action enum into `narsor-protocol`; the
  engine preserves its existing public name as a re-export.
- Removed the API discovery and connection modules' direct use of persistence
  APIs and `StatePaths`. Discovery now accepts a state-root path, reads a
  client-owned projection of the established `daemon.json` record, and owns
  the protected-file check, PID liveness probe, and atomic spawn-lock write.
- Moved detached sibling-server launch mechanics under `src/api/launch.rs`,
  where discovery uses them; retained the previous daemon module path as a
  compatibility re-export for existing callers.
- Added `DaemonDiscoveryRecord` to `narsor-protocol` and changed the client to
  decode the server's persisted discovery record through that shared contract.
  The process suite exercises compatibility with records written by the
  server.
- Moved conductor proposal and undo response types to `narsor-protocol`;
  conductor implementation modules re-export the protocol values, and the
  HTTP client no longer imports conductor implementation types for these
  endpoints.
- Moved the human input delivery payload into `narsor-protocol`; `ApiClient`
  now accepts the wire DTO, while the server adapter restores the internal
  delivery and marks it as an operator answer. The non-wire `answered_by`
  field remains absent from the DTO.
- Moved claim, owner-view, identity-response, and owner-binding wire values
  into `narsor-protocol`; auth handlers retain the record-to-view adapter and
  public auth paths re-export the protocol contracts. `ApiClient` no longer
  imports auth implementation types.
- Moved material manifest, origin, announcement, list, view, summary, upload
  response, and content-type contracts into `narsor-protocol`. `ApiClient`
  now uses these wire values directly and verifies downloaded blobs without
  calling into the materials storage module.
- Moved the serialized execution event model and its wire-preserving
  descriptions into `narsor-protocol`; `narsor::domain::event` remains a
  compatibility re-export. `ApiClient` now imports `Event` and `EventData`
  directly from the protocol package.
- Moved the owner-facing `CapabilitiesView` DTO into
  `narsor-protocol::controller`; the server wire module keeps a compatibility
  re-export and `ApiClient` imports the protocol type directly.
- Moved the default-backend view and update request into
  `narsor-protocol::controller`; the server converts its internal `BackendId`
  to the wire string, preserving the serialized `override` field name and
  value. `ApiClient` no longer imports these DTOs from the root controller
  implementation module.
- Moved workflow listing/validation and repository listing/creation contracts
  into `narsor-protocol::controller`. The server now maps its internal
  `ParamType` to a protocol-owned input-type enum; CLI and TUI render the
  protocol input type directly. `ApiClient` imports these contracts from
  protocol rather than the root controller module.
- Moved controller enrollment and listing contracts (`CreateEnrollmentRequest`,
  `EnrollmentResponse`, `ControllerView`, and `ControllerListResponse`) into
  `narsor-protocol::controller`. The server converts internal backend IDs to
  protocol strings at its adapter boundary, and `ApiClient` imports these
  contracts directly from protocol.
- Moved the conversation-page, execution-events, and conductor-read query
  values into `narsor-protocol::controller`. The server wire module re-exports
  them, and `ApiClient` imports them directly from protocol.
- Moved the owner task-move request and its tagged target into
  `narsor-protocol::controller`. Task anchors are opaque strings at the wire
  boundary; the server validates and parses them into internal `TaskId`
  values before applying a move. CLI and TUI adapters convert their internal
  move targets explicitly.
- Moved `OwnerCommand`, `OwnerCommandRequest`, and `SubmitInputRequest` into
  `narsor-protocol::controller`. The orchestration model re-exports the shared
  command enum, and the server converts the protocol input delivery DTO to the
  engine delivery type before queueing it. `ApiClient` imports both request
  contracts directly from protocol.
- Moved `OperatorCommand` and `SubmitOperatorCommandRequest` into
  `narsor-protocol::controller`. The orchestration model re-exports the command
  enum (including its readable description), and `ApiClient` imports the
  request contract directly from protocol.
- Moved `ExecutionEventsResponse` into `narsor-protocol::controller`; it now
  composes only protocol-owned event, source, and unavailability values.
  `ApiClient` imports this response directly from protocol.
- Moved task creation, launch, workspace, and requested resource profile
  contracts into `narsor-protocol::controller`. The server converts the
  protocol resource value to its internal `ResourceProfile` at launch
  resolution, and `ApiClient` imports `CreateTaskRequest` directly from
  protocol.
- Moved the durable `TaskLifecycle` enum into `narsor-protocol::controller`
  and retained its existing orchestration import path as a re-export. Its
  snake-case storage values and archival classification are covered by
  protocol tests.
- Moved the pure orchestration wire values `Observation`, `InputStatus`,
  `OperatorCommandStatus`, `QueueReason`, `AttentionKind`, and `AttentionItem`
  into `narsor-protocol::controller`; the orchestration model re-exports them
  for internal compatibility.
- Added `narsor-server` and `narsor-runner` process entry points. Discovery now
  starts the sibling server; the local backend and Kubernetes Pods start the
  standalone runner executable.
- Split the root package's executable targets: `narsor` is the TUI and
  `narsor-cli` owns operator commands. The CLI no longer exposes a `tui` or
  `runner` subcommand.
- Added the `narsor-controller` process entry point and changed the Kubernetes
  Deployment to invoke it directly; controller identity administration stays
  in `narsor-cli`.
- Removed the CLI's in-process server start path; `narsor-cli daemon` now
  requires the explicit stop operation, and config-startup process tests start
  the standalone `narsor-server` binary.
- Updated process and manifest tests to resolve workspace binaries explicitly.
- Updated the README's CLI/TUI invocation examples and workspace build/install
  instructions; the remote-backend architecture diagram now names
  `narsor-server`.

## Verification evidence

- `cargo check --workspace --all-targets` passes.
- `cargo build --workspace --bins` passes.
- `cargo metadata --no-deps --format-version 1` passes; the workspace contains
  the client, protocol, server, runner, controller, and controller-runtime
  packages.
- `cargo test -p narsor-controller-runtime`: 3 passed, including credential
  reuse, missing enrollment code, and `0600` persistence.
- Kubernetes controller enrollment integration test passes against the
  controller CLI adapter.
- `cargo test -p narsor-protocol`: 17 passed, including summary, chat, timeline,
  annotation, and material DTO wire assertions.
- Material storage/runner suite: 10 passed; controller protocol suite: 83
  passed, 1 ignored; API client blob order and digest tests: 2 passed.
- `cargo check --workspace --all-targets` passes after the material DTO
  extraction.
- Auth integration suite: 20 passed; API suite: 67 passed after extracting the
  auth wire types.
- Human-input suite: 28 passed; API suite: 67 passed after the delivery DTO
  migration.
- Conductor chat implementation suite: 24 passed; API suite: 67 passed after
  moving the proposal and undo response types.
- Human-input regression suite: 28 passed; API integration suite: 67 passed
  after the action enum extraction.
- `cargo check --workspace --all-targets` passes after chat DTO extraction.
- API integration suite: 67 passed; daemon lifecycle suite: 3 passed, 18
  ignored; conductor-failure suite: 12 passed, 1
  ignored; the ignored standalone-server recovery case passes when run directly.
- Timeline extraction regression coverage: all 24 `engine_recovery` tests pass.
- `cargo test --test api_discovery -- --include-ignored --test-threads=1`: all
  8 discovery tests pass, including standalone daemon startup and race cases.
- CLI/config/TUI target suites: all non-ignored tests passed; executable help
  tests verify the TUI and CLI targets expose separate command surfaces.
- Config startup: 7 normal tests and all 14 ignored standalone-server tests
  pass; bin-target tests: 3 passed, including proof that `narsor-cli daemon`
  cannot start the server in-process.
- API discovery's six ignored process tests pass.
- Daemon lifecycle/recovery's 18 ignored process tests pass.
- Backend contract: 37 passed, including local runner process cases.
- Kubernetes adapter: 39 passed, 1 live-cluster test ignored.
- Materials: 10 passed.
- Runner process suite: 26 passed on isolated rerun after one transient timeout
  in the first full run.
- TUI without-terminal discovery and configured log-file tests pass; the
  standalone controller help and Deployment command test pass.

## Historical progress notes

The notes below are chronological and describe intermediate states. The
current completion status is recorded at the end of this file.

- `narsor-server` and `narsor-runner` still depend on the root `narsor` package;
  implementation ownership and the client dependency graph are not yet split.
- `cargo tree -p narsor --edges normal --depth 1` still lists server, auth,
  persistence, and execution dependencies directly under the root package;
  separate executable targets alone do not satisfy R6.
- `cargo tree -p narsor-protocol --edges normal` contains only `chrono`,
  `serde`, and `serde_json`; the protocol package itself is independent of the
  root implementation.
- Protocol extraction is partial: `ApiClient` still imports controller
  protocol contracts from the root module and orchestration `TaskEvent`. The
  enrollment/list, capabilities, default-backend, workflow, and repository
  contracts have moved, but remaining imports need to be assessed and moved
  into protocol/client contracts where their dependency cost and semantics
  allow it.
- The three process packages (`narsor-server`, `narsor-runner`, and
  `narsor-controller`) still directly depend on root `narsor`; this is visible
  in each package's depth-one normal dependency tree.
- `narsor-controller-runtime` now owns generic credential loading, enrollment,
  and private credential persistence; controller reconciliation still has not
  moved into it.
- The controller loop supervision policy and restart/shutdown runner now live
  in `narsor-controller-runtime`; the root backend module re-exports that
  runtime API while retaining the reconciler itself.
- The runner command owns its process exit-code mapping and no longer imports
  `crate::cli`; this removes a frontend-to-frontend source dependency as a
  preparatory step for moving the runner module into its process package.
- Moved the runner implementation sources under `crates/narsor-runner` and
  added a package library facade over the current core modules. The root crate
  now retains only a compatibility module for `HarnessHome` and `HomeMode`,
  which are owned by `narsor::config` and consumed by local backend setup.
- The `narsor-runner` package still depends on root `narsor` for Engine,
  persistence, workflow, and controller protocol implementations; the source
  move establishes process ownership but does not yet satisfy R6.
- The root package remains a combined library and binary package rather than a
  client-only `narsor` package: its normal dependency graph still includes
  server implementation dependencies such as `axum` and the engine/store
  modules. `narsor-cli` is a binary target of that same package, not a separate
  Cargo package. `narsor-server`, `narsor-runner`, and `narsor-controller`
  each still depend directly on root `narsor`, so the central client/process
  dependency boundary and implementation ownership are not complete.
- After the runner source move, `cargo check --workspace --all-targets` passed;
  runner package unit tests passed (9), runner process tests passed (26),
  config resolution tests passed (21), and material runner tests passed (10).
- `cargo build -p narsor-runner` and `cargo build -p narsor --bins` both pass
  after the source move; `git diff --check` passes.
- The two relocated HOME-setting tests pass in `narsor`; the three binary-target
  tests pass; `cargo fmt --all -- --check` and `cargo metadata` pass. Metadata
  confirms `narsor-runner` now has both a library target and its process binary.
- `cargo tree -p narsor-runner --edges normal --depth 1` still shows `narsor`
  as a direct dependency. The client package also still directly lists server,
  persistence, engine, and UI dependencies, so the key R6 boundary remains
  open despite the runner source ownership move.
- `cargo test -p narsor-controller-runtime`: 5 passed, including the new
  supervisor restart and bounded-shutdown tests; the existing controller
  supervision integration suite passes all 4 tests.
- The controller credential adapter now lives in the shared backend
  controller module instead of the CLI module. Both the CLI command and the
  local server reconciler use the shared adapter, and daemon runtime no longer
  imports `crate::cli::controller`. Credential persistence still delegates to
  `narsor-controller-runtime` and retains its `0600` behavior.
- After relocating the controller enrollment adapter: workspace check passed;
  controller supervision tests passed (4), Kubernetes adapter tests passed
  (39, 1 live-cluster test ignored).
- After moving the execution event model into protocol: workspace check passed;
  protocol tests passed (18), API tests passed (67), SSE tests passed (8),
  and `cargo tree -p narsor-protocol --edges normal` showed only chrono, serde,
  serde_json, and uuid dependencies.
- After extracting the remaining pure orchestration summary values into
  protocol: `cargo check --workspace --all-targets` passed, all 31 protocol
  tests passed, controller protocol tests passed (83, 1 ignored), and API tests
  passed (67). `cargo fmt --all -- --check` and `git diff --check` also passed.
- Moved the controller task timeline response into protocol. Its task ID is
  now an opaque wire string and runner status is `ExecutionStatusDto`; the
  server adapts its validated domain ID and execution status. Workspace check
  passed, protocol tests passed (32), controller protocol tests passed (83, 1
  ignored), API tests passed (67), and formatting plus `git diff --check`
  passed.
- Moved queued input/operator-command response and view structs into protocol;
  the server wire module retains only conversions from orchestration records.
  `ApiClient` imports the protocol responses directly. Workspace check passed,
  protocol tests passed (33), controller protocol tests passed (83, 1 ignored),
  API tests passed (67), and formatting plus `git diff --check` passed.
- Moved explicit stop signaling to `narsor-server stop`. `narsor-cli daemon
  stop` now invokes the sibling server with the selected root/config; the CLI
  no longer reads `DaemonInfo` or signals the daemon PID. Workspace check and
  server/CLI build passed. No-discovery and stale-PID lifecycle cases passed;
  the live daemon stop case passed when rerun outside the sandbox after the
  sandbox denied loopback bind with `Operation not permitted`. All 3
  binary-target tests passed; README now documents the server lifecycle form;
  formatting and diff checks passed. The live stop test passed again after
  preserving the prior CLI stdout/stderr and exit-code behavior.
- Moved validated `TaskId`/`BackendId` identifiers and rendered workflow
  display values/sanitizers into `narsor-protocol`, keeping their domain paths
  as re-exports. Identifier deserialization still enforces the original
  path-safe character rules. Workspace check passed; protocol tests passed
  (38), controller protocol tests passed (83, 1 ignored), orchestration tests
  passed (106), API tests passed (67), and formatting/diff checks passed.
- Made the protocol execution-status enum the canonical domain status type;
  the domain module keeps its existing name as a re-export, and duplicate
  domain↔DTO conversions are gone. Workspace check passed; protocol tests
  passed (38), API tests passed (67), engine recovery tests passed (24), and
  formatting/diff checks passed.
- Moved normalized observation and queue views plus their supporting resource,
  resume, cursor, and runner-problem summary values into protocol; the
  orchestration model preserves its existing names as re-exports, and the
  server adapter retains queue-record conversion. Workspace check passed;
  protocol tests passed (39), controller protocol tests passed (83, 1
  ignored), orchestration tests passed (106), API tests passed (67), and
  formatting/diff checks passed.
- After moving `CapabilitiesView`: workspace check passed, protocol tests
  passed (19), API tests passed (67), formatting and `git diff --check` passed.
- After moving the default-backend DTOs: workspace check passed, protocol tests
  passed (20), API tests passed (67), CLI smoke tests passed (26; 34 process
  tests ignored), formatting and `git diff --check` passed.
- After moving workflow and repository contracts: workspace check passed;
  protocol tests passed (24), API tests passed (67), CLI control-text tests
  passed (9), TUI state tests passed (343), and TUI render tests passed (147).
- After moving controller enrollment/list contracts: workspace check passed;
  protocol tests passed (25), API tests passed (67), controller protocol tests
  passed (83; 1 ignored), formatting and `git diff --check` passed.
- After moving the control-plane query DTOs: workspace check passed; protocol
  tests passed (26), API tests passed (67), and controller protocol tests
  passed (83; 1 ignored).
- After moving the task-move request: workspace check passed; protocol tests
  passed (27), API tests passed (67), controller protocol tests passed (83;
  1 ignored), TUI state tests passed (343), and formatting/diff checks passed.
- After moving owner-command and input-submission contracts: workspace check
  passed; protocol tests passed (28), API tests passed (67), controller
  protocol tests passed (83; 1 ignored), runner tests passed (26), and TUI
  state tests passed (343); formatting and `git diff --check` passed.
- After moving `OperatorCommand` and its submission request into protocol:
  workspace check passed; protocol tests passed (28), API tests passed (67),
  controller protocol tests passed (83; 1 ignored), runner tests passed (26),
  and TUI state tests passed (343); formatting and `git diff --check` passed.
- After moving `ExecutionEventsResponse` into protocol: workspace check passed;
  protocol tests passed (28), API tests passed (67), controller protocol tests
  passed (83; 1 ignored), formatting and `git diff --check` passed.
- After moving task-creation contracts into protocol: workspace check passed;
  protocol tests passed (29), API tests passed (67), controller protocol tests
  passed (83; 1 ignored), formatting and `git diff --check` passed.
- After moving `TaskLifecycle` into protocol: workspace check passed; protocol
  tests passed (30), API tests passed (67), controller protocol tests passed
  (83; 1 ignored), formatting and `git diff --check` passed.
- After moving `OperatorCommand` and its submission request into protocol:
  workspace check passed; protocol tests passed (28), API tests passed (67),
  controller protocol tests passed (83; 1 ignored), runner tests passed (26),
  and TUI state tests passed (343); formatting and `git diff --check` passed.
- Test ownership, deployment/build artifacts, and the remaining documentation
  still need a full audit against U2–U7 and the Verification Contract.
- Moved `RunnerView`, `TaskSummary`, and task list responses into protocol;
  pending input form data is now opaque JSON at the wire boundary and converted
  to the engine form only by the TUI when opened. Workspace check passed;
  protocol tests passed (39), controller protocol tests passed (83, 1
  ignored), API tests passed (67), TUI state passed (343), TUI render passed
  (147), CLI task text passed (24), and formatting/diff checks passed.
- Moved durable command receipts, task events, and control event envelopes into
  protocol, preserving tagged event serialization and unknown-event fallback.
  Workspace check passed; protocol tests passed (40), controller protocol
  tests passed (83, 1 ignored), orchestration tests passed (106), API tests
  passed (67), and formatting/diff checks passed.
- Full `cargo test --workspace` passed. All four requested package builds passed
  (`narsor --bins`, server, runner, controller), and workspace Clippy passed
  with `-D warnings` after removing newly surfaced `Copy` clones and fixing
  clippy findings. The first parallel `--include-ignored` run hit one flaky
  SSE timeout (the test passed alone); the serial include-ignored retry later
  failed three opt-in `cli_smoke` tests because tasks reported concurrent
  driving/recovery conflicts. Those opt-in cases are not green and need
  isolation or diagnosis before the verification contract is satisfied.
- The package boundary is still not enforced: `narsor-server`,
  `narsor-runner`, and `narsor-controller` still depend on the root `narsor`
  package, which retains client, server, engine, persistence, and runner
  dependencies. R6 and the package-graph portions of U1–U7 remain incomplete.
- Moved the owner conversation response and its attempt summary into protocol;
  `ApiClient` now imports the response from `narsor-protocol`. Protocol tests
  passed (41), controller protocol tests passed (83, 1 ignored), API tests
  passed (67), CLI task-text tests passed (24), workspace check passed, and
  formatting/diff checks passed.
- Isolated one of the three include-ignored CLI smoke failures and confirmed it
  fails by itself with an “already being driven” conflict. These tests are
  deliberately ignored because their legacy conductor/recovery path is not
  wired to scheduler-admitted runner tasks; they require migration to the new
  task/API model before the include-ignored contract can pass.
- Moved runner conversation availability into protocol alongside the owner
  conversation response and summary. Added a tagged-shape round-trip test.
  Workspace check passed; protocol tests passed (42), controller protocol
  tests passed (83, 1 ignored), and formatting/diff checks passed.
- Moved the executable daemon lifecycle into `narsor-server` and made the
  server binary call its own package runtime. The root daemon module now only
  contains client-side server-launch support. The old, pre-existing
  `src/daemon/runtime.rs` worktree file is preserved but no longer compiled;
  root integration tests reach the moved periodic-pass, startup-recovery, and
  drain seams through a dev dependency on `narsor-server`. Workspace check
  passed; the three focused seam tests passed; all 18 ignored daemon lifecycle
  process tests passed, including start/stop and shutdown ordering. The normal
  daemon lifecycle suite passed (3; 18 ignored), controller protocol passed
  (83; 1 ignored), and `cargo test -p narsor-server` passed.
- The pre-existing uncommitted daemon changes in `src/cli/daemon.rs`,
  `src/daemon/mod.rs`, and `src/daemon/runtime.rs` are preserved and excluded
  from implementation commits.
- Moved the API listener, `ApiHandle`, `ApiServices`, router, and REST/SSE route
  implementations into `narsor-server::api`; the root `narsor::api` module now
  contains client transport, discovery, and client DTO facades. Server-side
  conductor and harness-selection conversions moved with the routes, and the
  API integration suites now import server setup from the server package.
  Workspace check passed; API tests passed (67), auth (20), SSE (8), materials
  (10), controller protocol (83; 1 ignored), and all 18 ignored daemon
  lifecycle process tests. Workspace Clippy, formatting, and diff checks
  passed.
- Moved the owner authentication gate into `narsor-server::auth_gate`, and the
  controller/runner HTTP routers into `narsor-server::controller_protocol::http`.
  The protocol error enum no longer implements Axum's `IntoResponse`; the
  response mapping is local to the server HTTP adapter, while the server API
  maps protocol errors into its own API error type. Workspace check passed.
- Removed Axum, `axum-extra`, `tokio-stream`, and `tower-http` from the root
  package's normal dependencies. Axum remains a root dev-dependency because
  integration test helpers build in-process mock routers. `cargo check
  --workspace --all-targets` passed, and `cargo tree -p narsor --edges normal
  --depth 1` no longer lists those HTTP packages.
- R6 remains incomplete: the root client package still contains engine,
  persistence, orchestration, and server-adjacent modules, and the server,
  runner, and controller packages still depend on root `narsor`. The current
  dependency cleanup only removes the extracted HTTP stack; it does not yet
  enforce client/server implementation separation.
- Regression checks after the HTTP ownership move passed: `cargo test
  -p narsor-server` (1), `cargo test --test api` (67), `cargo test --test auth`
  (20), and `cargo test --test controller_protocol` (83 passed, 1 ignored).
  `cargo fmt --all` was applied; the formatting check and `git diff --check`
  pass.
- `cargo clippy --workspace --all-targets -- --deny warnings` passed after the
  move. The focused tests above exercise the server-local HTTP error mapping,
  owner auth gate, and controller/runner route wiring.
- U4 progress: the legacy `narsor-cli run` command now obtains catalogue or
  file workflow input schemas from server control endpoints and submits via
  `POST /tasks`; it no longer resolves/loads a workflow with client-side
  `WorkflowResolver`/`tasks`. CLI legacy and remote task input commands now
  construct `HumanInputDeliveryDto` directly instead of using the engine's
  internal `HumanInputDelivery`. The source comments describe this API ownership.
  Existing tests were inspected and used unchanged: the unknown-name case,
  missing-required-input case, root-catalogue run case, task creation case,
  uninterrupted validate/run/input/show path, and legacy input delivery all
  pass (the process cases were explicitly run with `--ignored`). The
  `cli_smoke` suite passed its 26 non-ignored tests (34 remain ignored by their
  existing process/conductor reasons). Workspace check, Clippy with warnings
  denied, formatting, and diff checks pass. No new tests were needed because
  existing process coverage exercises the end-to-end API path.
- Continued U4 by moving standalone `narsor-cli validate` to the server's
  workflow-list and file-validation endpoints. The protocol workflow views
  now carry defaulted `state_count` and `initial_state` fields, preserving the
  CLI's existing success detail while keeping YAML parsing server-side.
  Existing protocol/UI fixtures were updated for the additive fields, and the
  wire test asserts them. The three CLI validation tests pass (valid file,
  all five violations, built-in); all 42 protocol tests, 343 TUI state tests,
  and 147 TUI render tests pass. Workspace check, warnings-denied Clippy,
  formatting, and diff checks pass.
- Repointed additional TUI/CLI presentation imports from orchestration and
  controller modules to protocol types, and moved the transcript truncation
  marker into protocol. After this import pass, `cargo check
  --workspace --all-targets`, warnings-denied workspace Clippy, formatting,
  and diff checks pass; `cargo test --test tui_state` passes (343) and
  `cargo test --test tui_render` passes (147).
- A focused client import audit still finds direct implementation crossings:
  `src/cli/commands.rs` imports engine pending-input, repository registry, and
  workflow resolver; `src/cli/controller.rs` imports Kubernetes/backend
  implementations; `src/cli/remote.rs` imports orchestration and workflow
  resolver; `src/tui/input_form.rs` imports engine continuation/pending-input
  and domain form types; `src/tui/task_app/waiting.rs` imports engine pending
  input. The root `narsor` package also still exposes implementation modules,
  and server/runner/controller depend on it. Thus U4 and R6 are not complete.
  `cargo tree -p narsor --edges normal --depth 1` confirms the client still
  has runtime dependencies including `narsor-controller-runtime`; removing
  the HTTP dependencies alone does not provide the required package boundary.
- Replaced the TUI's orchestration `MoveTarget` with protocol
  `MoveTaskTarget` end-to-end, including move notifications and state tests.
  This keeps the value sent to the API the same protocol value held by the
  client. `cargo test --test tui_state` passes (343), and `cargo fmt --all`
  passes. The remaining loss/resume presentation helpers in that file still
  come from orchestration, and the larger direct-import/package-boundary list
  above remains open.
- Moved task-loss summary and exhaustion-reason helpers into protocol display
  code; orchestration keeps re-exports for its existing internal consumers,
  while the CLI and TUI use the protocol module directly. The remote task CLI
  now gets an omitted workflow version from the server's workflow endpoint
  instead of resolving and parsing the local catalog; explicit versions still
  bypass that lookup. The existing remote CLI integration case now exercises
  server-selected version lookup. Workspace check passes, and the targeted
  CLI process test passes (1); orchestration (106), CLI control-text (9), and
  TUI state (343) suites also pass after the protocol helper move.
- The focused client source audit now has no remaining orchestration imports:
  `src/cli/remote.rs` and CLI/TUI task presentation use protocol command,
  event, move-target, and loss-state types directly. Remaining CLI/TUI
  implementation imports are pending-human-input types in `commands.rs`,
  `input_form.rs`, and `waiting.rs`; registry/workflow utilities in
  `commands.rs`; and Kubernetes/backend implementation in `cli/controller.rs`.
  The server/runner/controller Cargo dependency on root `narsor` remains the
  main R6 blocker. Warnings-denied workspace Clippy, formatting, and diff
  checks pass after these changes.
- Moved the standalone Kubernetes controller argument parser and process
  loop from `src/cli/controller.rs` into `crates/narsor-controller/src/`;
  the binary now calls its own package library, and the root CLI no longer
  declares the controller implementation module. The credential enrollment
  regression now exercises the shared backend helper directly. Workspace
  check passes and the focused enrollment/0600 test passes (1). The controller
  package still depends on root `narsor` for backend implementation, so U6's
  runtime ownership and R6 are not yet complete.
- Ran the standalone controller binary with `--help`; it starts and exposes
  the expected Kubernetes/controller flags. Warnings-denied workspace Clippy
  passes after the command move.
- Removed the CLI renderer's direct engine `PendingHumanInput` dependency;
  it now formats the serialized pending-input value carried by the API
  response. The existing ignored end-to-end validate/run/input/show case
  passes (1), preserving the user's form summary path. Workspace check passes.
- Final source and artifact checks for this slice pass: Clippy with warnings
  denied, formatting, and `git diff --check`. The latest client import scan
  now shows TUI input form/waiting as the remaining engine imports, plus
  top-level CLI repository/workflow commands. Cargo's normal root-client tree
  still includes `narsor-controller-runtime`, and the server, runner, and
  controller binaries still link root `narsor`; the DoD package graph is
  therefore still unachieved.
- Added the protocol-owned `PendingHumanInputView` and moved the client's
  pending form rendering to it. The server now projects the persisted engine
  record into this independent wire type, and human-answer DTO conversions
  from wire to engine are in server-side adapters rather than root client DTO
  code. The wire keeps the flattened input fields and accepts legacy string
  output labels. Workspace check passes; protocol tests pass (43), API tests
  pass (67), and TUI state tests pass (343); the combined API/TUI command also
  completed successfully, including the render suite. Engine imports are now
  absent from the TUI modules in the focused source scan. Package graph/R6,
  remaining CLI registry/workflow ownership, and the separate process tests
  remain outstanding.
- Routed top-level `narsor-cli repo` add/list/show/remove through the same
  authenticated repository API used by task commands, then removed the CLI's
  direct `RepositoryRegistry` implementation calls. The existing repository
  CLI regression tests pass (9), including duplicate IDs, unreadable and
  unknown IDs, and field display; workspace check passes. Dedicated workflow
  list/show/eject commands still use the workflow resolver, and the package
  graph remains incomplete.
- Re-ran the workspace quality gates after the latest client/protocol changes:
  `cargo check --workspace --all-targets`, warnings-denied Clippy, formatting,
  and `git diff --check` all pass. A fresh `cargo test --workspace` attempt
  could not complete because the workspace filesystem reached 100% usage
  during parallel test linking (`No space left on device`); available space
  was 1.3 MB afterward. The previously recorded targeted suites remain the
  usable test evidence for this continuation.
- Rechecked normal dependency trees: `narsor` still depends directly on
  `narsor-controller-runtime` and the broad implementation dependency set;
  at that point `narsor-server`, `narsor-runner`, and `narsor-controller` also
  depended directly on root `narsor`. This confirmed the central R6 package
  boundary was unfinished.
- Began the package extraction by adding `narsor-engine`, whose library now
  owns the existing implementation source tree, and repointed the server,
  runner, and controller dependencies to that package. Their normal depth-one
  trees now name `narsor-engine`, not root `narsor`. Metadata resolves and
  `cargo check -p narsor-engine -p narsor-server -p narsor-runner
  -p narsor-controller --lib --bins` passes. Root `narsor` is not yet a
  client-only crate and still compiles the same source independently; its
  normal dependency tree still includes the implementation dependencies.
  The first full workspace all-target check exposed integration tests that
  currently mix types compiled from root `narsor` with the new engine package
  (for example, `Engine` and `ProtocolService` become distinct Rust types).
  Those tests need to move to the appropriate package/client contracts as the
  root library is narrowed. Workflow list/show/eject also still access the
  root resolver. The plan's Definition of Done remains unmet.
- Started narrowing the root library toward a client-only crate by adding an
  explicit `src/client_lib.rs` crate root that exposes the TUI/CLI, client
  transport, and protocol types while omitting server and engine modules.
  Moved the wire-owned `StartTaskResponse`, `ExecutionSummary`, and
  `ExecutionListResponse` structs into `narsor-protocol::api`; the existing
  engine-side `From<&WorkflowExecution>` projection remains in the engine
  crate. The root client build now provides concrete feedback for the
  remaining boundary work: legacy conductor and control-plane views still
  depend on engine types, config parsing still imports engine/auth/persistence
  helpers, and CLI material/workflow commands still depend on implementation
  modules. `cargo check -p narsor --lib --bins` currently fails on those
  unresolved imports, so the client crate extraction is actively incomplete.

- Continuation update: the current `src/client_lib.rs` and client command
  paths now compile with `cargo check -p narsor --lib --bins`; the earlier
  unresolved-import observation above is stale. `cargo tree -p narsor --edges
  normal --depth 1` contains `narsor-api-client`, `narsor-protocol`, and
  `narsor-shared`, with no engine/server/runner/controller package. Renamed
  the direct engine dependency alias in the server, runner, and controller
  manifests and source references from `narsor` to `narsor-engine`, making
  their implementation ownership explicit. `cargo check -p narsor-server
  -p narsor-runner -p narsor-controller --all-targets` and
  `cargo check --workspace --all-targets` pass. `cargo metadata --no-deps`,
  `cargo fmt --all -- --check`, and `git diff --check` pass. The controller
  runtime still imports backend/orchestration/auth/client implementation
  types from `narsor-engine`; U6/R7 remains open and needs a deeper contract
  extraction before the workspace boundary is complete.
- Removed the now-empty `engine` feature from the client package and its
  conditional protocol-to-client conversion in `tui/task_app/history.rs`.
  `cargo check -p narsor --all-targets`, `cargo fmt --all -- --check`, and
  `git diff --check` pass without the previous unexpected-feature warnings.
- Continued R7 by moving `ReportResponse` into `narsor-protocol` and updating
  `narsor-controller-runtime` to import controller wire values, task IDs, and
  the shared client error directly from `narsor-protocol`/`narsor-shared`.
  The engine's controller protocol facade re-exports the response for existing
  server and controller consumers. Verification: controller runtime/server/
  controller all-target check passes; protocol tests pass (44), and
  controller protocol tests pass (83 passed, 1 ignored). Formatting and diff
  checks pass. `narsor-controller-runtime` still depends on engine backend,
  credential, and authenticated-client implementations; those remain U6/R7
  work.
- Fresh U7 verification after these changes: `cargo test --workspace --
  --test-threads=1` passed with exit code 0 outside the sandbox; the sandboxed
  attempt failed because tests could not bind localhost. `cargo clippy
  --workspace --all-targets -- -D warnings`, all four required package builds,
  `cargo fmt --all -- --check`, `cargo metadata --no-deps`, and
  `git diff --check` pass. The full ignored-test run remains documented above:
  three scheduler follow-up cases fail because that separate behavior is not
  implemented, and the live Kubernetes smoke test requires a cluster.

## Completion status at the prior checkpoint

- The workspace contains `narsor`, `narsor-protocol`, `narsor-engine`,
  `narsor-server`, `narsor-runner`, `narsor-controller`, and
  `narsor-controller-runtime`; the README and operations guide describe the
  process split. U6 remains incomplete: the Kubernetes adapter belongs to
  `narsor-controller`, and the shared reconciler/testkit now belong to
  `narsor-controller-runtime`; however, that runtime still depends on engine
  types, and controller protocol client/contracts still live in `narsor-engine`.
- The normal dependency tree for `narsor` contains `narsor-protocol` and no
  engine, server, runner, or controller implementation package.
- Earlier workspace verification after the package and workflow-source changes:
  `cargo check --workspace --all-targets`, `cargo build --workspace --bins`,
  warnings-denied workspace Clippy, formatting, metadata, and
  `git diff --check` passed. The full serial test suite later showed one
  repeatable runner failure (`a_paused_task_resumes_on_its_own_storage_without_recloning`)
  that passed when run by itself; the rest passed with that case skipped.
- Targeted post-review regressions pass: protocol (44), API (67), auth (20),
  binary targets (3), and built-in workflow CLI (16; 2 ignored). Workflow
  source endpoint coverage now includes owner-token refusal and network-mode
  refusal to read user workflow files.
- The plan's `--include-ignored` run exposed three previously ignored
  scheduler-admitted conductor/recovery cases that are still unsupported by
  the scheduler path. On the current branch the exact command fails in
  `tests/cli_smoke.rs` at `conductor_status_prints_what_the_conductor_is_doing`,
  `recover_restarts_a_failed_step_shown_as_a_problem`, and
  `the_cli_and_the_api_share_the_conductors_chat`; their `#[ignore]` reasons
  in the baseline identify the unified task scheduler follow-up. The normal
  workspace suite passes. A second include-ignored run skipped those three
  cases and the live-cluster smoke test; the other ignored process/lifecycle
  tests passed. The live-cluster smoke test remains environment-gated.
- Review follow-up: legacy workflow responses now preserve an unknown
  `has_builtin` state so `workflow eject` reports an old server instead of
  claiming a built-in is missing. A CLI target test explicitly rejects
  `narsor-cli daemon start`.
- Continued U6 by moving the Kubernetes REST adapter, resource manifests,
  object types, and configuration under `crates/narsor-controller/src/kubernetes`.
  The engine no longer declares or exports a Kubernetes backend; controller
  process code and adapter tests now use the controller-owned module. Added
  integration tests that execute the actual `narsor-controller` binary for
  help output and missing-argument exit status. Verification: controller
  package all-target check, engine all-target check, Kubernetes adapter tests
  (39 passed, 1 live-cluster test ignored), and controller binary tests (2
  passed).
- Continued U6 by moving the generic reconciler and backend contract testkit
  into `narsor-controller-runtime`. Removed the engine's dependency on that
  runtime; the server and standalone controller now import the reconciler from
  its shared package. Verification: workspace all-target check, backend
  contract tests (37 passed), controller supervision (4 passed), runtime unit
  tests (6 passed), Kubernetes adapter (39 passed, 1 live-cluster test
  ignored), and controller binary tests (2 passed).
- Remaining U6/R7 work: move backend-neutral contracts and controller/runner
  protocol DTOs/client transport out of engine so `narsor-controller-runtime`
  no longer depends on the engine implementation and the wire contract is
  independent of orchestration models.
- Continued R7 by replacing the engine-owned `RunnerSnapshotRequest` alias
  with an engine-independent DTO in `narsor-protocol`. The HTTP service now
  converts JSON pending-input forms to the engine's durable form model only
  after decoding the protocol request; the runner client performs the reverse
  conversion before sending. Also moved simple controller response, lease,
  runner directive, and poll-query DTOs into the protocol package. Verification:
  `cargo check --workspace --all-targets`, warnings-denied workspace Clippy,
  and controller protocol integration tests (83 passed, 1 ignored). The
  remaining controller and runner wire DTOs and client transport are still
  engine-owned, so R7 is incomplete.
- Continued R7 by moving enrollment, lease, command-query/acknowledgement,
  runner-credential, report, poll, and runner acknowledgement request/response
  DTOs into `narsor-protocol`. The enrollment signing helper now lives with
  engine auth and returns the protocol-owned DTO; no key implementation is
  part of the wire crate. `DesiredAction` also moved to the protocol package.
  Verification after this batch: workspace all-target check, warnings-denied
  workspace Clippy, controller protocol integration tests (83 passed, 1
  ignored), formatting, and `git diff --check` passed. The initial sandboxed
  test attempt could not bind local ports; the same test passed when rerun
  outside the sandbox. `DesiredCommand`, runner poll response payloads, and
  the controller/runner HTTP transport remain coupled to engine models and
  are outstanding R7 work.
- Continued R7 by moving `DesiredCommand`, `RunnerProgress`, and
  `CommandsResponse` into `narsor-protocol`. The sealed launch spec is an
  opaque JSON value in the wire DTO; the controller runtime deserializes and
  validates it into the engine model immediately before provisioning. The
  command response still has the same JSON shape. Updated integration
  assertions to decode the launch spec explicitly. Verification:
  workspace all-target check, warnings-denied Clippy, formatting,
  `git diff --check`, and controller protocol integration tests (83 passed,
  1 ignored; run with permission to bind local TCP ports). Runner poll
  response payloads and HTTP transport remain outstanding R7 work.
- Review coverage note: `tests/engine_support::workspace_binary` resolves
  sibling binaries from the workspace target directory. Package-scoped tests
  that use that helper must build workspace binaries first; the required
  workspace-wide test and build commands do so and passed.
- No files were staged or committed. The user confirmed that all current
  uncommitted paths belong to this refactor and are in scope.

## Continuation — 2026-10-13

- Completed U6's remaining dependency boundary. The backend-neutral
  `ExecutionBackend` contract and its records/errors now live in
  `narsor-controller-runtime::backend`; `narsor-engine` re-exports the types
  to preserve existing public paths. The runtime's normal dependency graph
  now contains protocol/shared and utility crates, with no engine edge. An
  architecture integration test asserts this boundary.
- `WorkloadLaunch.launch_spec` is now protocol JSON rather than an engine
  domain type. Runtime validates the versioned top-level envelope before
  issuing runner credentials; local and Kubernetes adapters deserialize the
  complete engine launch spec before creating resources. This preserves the
  prior pre-provision validation boundary without importing engine models
  into the runtime.
- The engine owns the concrete HTTP `ControllerClient` adapter and its
  credential/signing implementation; the runtime remains generic over
  `ControllerLink` and `EnrollmentClient`. Controller API wire DTOs,
  including runner poll/snapshot payloads, are owned by `narsor-protocol`.
- Split protocol API DTOs into `api/material.rs`, `api/timeline.rs`, and
  `api/human_input.rs`; `api.rs` re-exports them to preserve old import paths.
  Fixed the stale discovery-lock reaper race by serializing the inspect/reap/
  create sequence with an exclusive flock on a persistent guard file, and
  added an eight-thread regression test.
- Review follow-up applied: the maintainability recommendation to split
  `api.rs` and reliability finding for concurrent stale-lock reapers are
  resolved. The testing advisory about sibling binaries is covered by the
  workspace build and full workspace suite; package-scoped process tests
  should still be run after building workspace binaries.
- Verification on the final source state: `cargo check --workspace
  --all-targets`, `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo fmt --all -- --check`, `cargo metadata --no-deps`, and
  `git diff --check` pass. `cargo test --workspace -- --test-threads=1`
  passes. Targeted results also include API-client lock regression (1),
  protocol (44), runtime (6), backend contract (37), controller protocol
  (83 passed, 1 ignored), controller supervision (4), and daemon lifecycle
  (3 passed, 18 ignored). The Kubernetes live-cluster smoke remains ignored
  without a configured cluster.
- Dependency checks: `cargo tree -p narsor` contains protocol/client/shared
  packages but no engine/server/runner/controller implementation package;
  `cargo tree -p narsor-controller-runtime` contains no `narsor-engine`.
  U6 and R7 are complete. No files were staged or committed.
