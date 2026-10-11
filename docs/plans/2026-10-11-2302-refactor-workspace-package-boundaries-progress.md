# Workspace package boundaries — implementation progress

This note records implementation evidence without changing the implementation
plan. The plan remains the authority for scope and completion.

## Completed slices

- Established the Cargo workspace and protocol/server/runner/controller package
  scaffolding.
- Moved local REST request/response values, conversation page types, controller
  view values, and execution status wire values into `narsor-protocol`.
- Added `narsor-server` and `narsor-runner` process entry points. Discovery now
  starts the sibling server; the local backend and Kubernetes Pods start the
  standalone runner executable.
- Split the root package's executable targets: `narsor` is the TUI and
  `narsor-cli` owns operator commands. The CLI no longer exposes a `tui` or
  `runner` subcommand.
- Updated process and manifest tests to resolve workspace binaries explicitly.

## Verification evidence

- `cargo check --workspace --all-targets` passes.
- `cargo build --workspace --bins` passes.
- `cargo test -p narsor-protocol`: 7 passed.
- API integration suite: 67 passed.
- CLI/config/TUI target suites: all non-ignored tests passed; executable help
  tests verify the TUI and CLI targets expose separate command surfaces.
- API discovery's six ignored process tests pass.
- Daemon lifecycle/recovery's 18 ignored process tests pass.
- Backend contract: 37 passed, including local runner process cases.
- Kubernetes adapter: 39 passed, 1 live-cluster test ignored.
- Materials: 10 passed.
- Runner process suite: 26 passed on isolated rerun after one transient timeout
  in the first full run.

## Remaining plan work

- `narsor-server` and `narsor-runner` still depend on the root `narsor` package;
  implementation ownership and the client dependency graph are not yet split.
- Protocol extraction is partial: execution summaries, conductor payloads,
  controller protocol types, and several client-facing values still depend on
  internal root types.
- Controller runtime and binary packages remain scaffolding.
- Test ownership, deployment/build artifacts, and documentation still need a
  full audit against U2–U7 and the Verification Contract.
- The pre-existing uncommitted daemon changes in `src/cli/daemon.rs`,
  `src/daemon/mod.rs`, and `src/daemon/runtime.rs` are preserved and excluded
  from implementation commits.
