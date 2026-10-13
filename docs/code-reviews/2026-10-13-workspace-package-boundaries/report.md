# Workspace package boundaries review follow-up

Date: 2026-10-13  
Scope: workspace package-boundary refactor and fixes applied after review  
Base: `04c7b2ce12d0d48f774f7f3beac92bf806d59e1c`

## Findings applied

- **Reliability — stale lock reaper race:** fixed by holding an exclusive
  `flock` on a persistent guard file across stale-lock inspection, removal,
  and lock creation. Added an eight-thread regression test proving a stale
  reaper cannot remove the winning daemon lock. The guard explicitly uses
  `.truncate(false)` so reopening it preserves the shared lock inode.
- **Maintainability — oversized protocol API module:** moved materials,
  timeline, and human-input DTO families into focused modules under
  `narsor-protocol/src/api/`; public re-exports preserve existing
  `narsor_protocol::api::Type` paths.
- **Boundary audit — runtime depended on engine:** moved backend-neutral
  backend contracts into `narsor-controller-runtime::backend`, changed the
  launch payload to protocol JSON, and moved concrete HTTP client/enrollment
  adaptation into engine-owned code. Runtime is generic over its link and
  enrollment traits and has no engine dependency. An architecture test
  protects the graph.

## Verification

- `cargo check --workspace --all-targets` — passed.
- `cargo clippy --workspace --all-targets -- -D warnings` — passed.
- `cargo fmt --all -- --check`, `cargo metadata --no-deps`, and
  `git diff --check` — passed.
- `cargo test --workspace -- --test-threads=1` — passed.
- Targeted checks: API-client concurrent stale-lock test (1 passed), protocol
  (44), controller runtime (6), backend contract (37), controller protocol
  (83 passed, 1 ignored), controller supervision (4), and daemon lifecycle
  (3 passed, 18 ignored).
- The exact runtime dependency tree has no `narsor-engine` edge. The root CLI
  dependency tree has no engine/server/runner/controller implementation edge.

## Advisory and route coverage

The package-scoped process-test helper resolves sibling binaries in the
workspace target directory. Workspace build and full workspace tests pass;
standalone package test runs that use this helper should build workspace
binaries first.

The cross-model review was attempted through the configured Claude route
(requested model `claude-opus-5`, high effort) but ended at the peer harness
maximum-turn limit without a normalized result artifact. The independent
local adversarial fallback completed with no findings; peer output was not
used. Route model, effort, and receipt could not be independently verified.

Changes remain unstaged and uncommitted.
