# Follow-ups after the narsor-engine split

Status: open. No plan and no decision to implement yet.
Origin: `docs/plans/2026-11-15-001-refactor-split-narsor-engine-plan.md`
(steps 1–8 done) and the `/simplify` review of `crates/narsor-engine`.

Each item is independent of the others.

## Structure

### Rename `narsor-engine` to `narsor-control-plane`

After the split, `narsor-engine` holds only the control plane: `auth`,
`controller_protocol`, `environment`, `launch`, `materials` (store),
`observers` and `orchestration`, plus re-exports of `narsor-exec`,
`narsor-model`, `narsor-store` and `narsor-controller-client` under the old
module paths. The name no longer matches the content.

- Rename the crate and update `narsor-server`, `narsor-controller`,
  `tests/`, `docs/dev/components.md` and `AGENTS.md`.
- Optionally remove the re-exports once `narsor-server` and the tests import
  the lower crates directly.

### `narsor-controller` still links the whole control plane

`narsor-controller` uses from `narsor-engine` only `config`, `domain`,
`environment` (`BackendCapabilities`, `BackendKind`), `registry`,
`workspace` and `auth` (a doc link). `environment::backend` and the catalog
are control-plane configuration, but the controller needs only the backend
kinds and capabilities. Moving those types lower (to `narsor-model` or
`narsor-controller-client`) would let the controller drop `narsor-engine`.

## Duplication and efficiency

- **`github-copilot-sdk` default features** pull `native-tls`/OpenSSL into
  every binary that links `narsor-exec`. Use
  `default-features = false, features = ["runtime"]` after a check that
  `CliProgram::Resolve`
  (`crates/narsor-exec/src/engine/actions/harness/copilot_sdk.rs`) works
  without `bundled-cli`.
- **Conductor types twice**: `narsor-exec/src/conductor/{view,response,store}.rs`
  and `narsor-protocol/src/controller/conductor.rs`, bridged by a JSON round
  trip in `controller_protocol/service/reads/conductor.rs`.
- **`RunnerSnapshot` mirrors `RunnerSnapshotRequest`** field by field;
  `RunnerSnapshot::to_request`/`from_request`
  (`narsor-model/src/domain/runner_snapshot.rs`) copy 17 fields both ways.
- **SHA-256 → hex** is written in six places; one `digest` helper would do.
- **`read_dir` treating `NotFound` as empty** is pasted 14 times; add
  `read_dir_or_empty` to `narsor_store::atomic`.
- **`flock`** is implemented in five crates; keep one `FileLock` (now
  `narsor_store::lock::FileLock`) and reuse it, or move it to
  `narsor-shared`.
- **`tokio = { features = ["full"] }`** in `narsor-exec` and
  `narsor-engine` overrides narrower feature lists in consumers.
- **`conductor::store::append_chat`** reads and parses the whole chat twice
  per append (O(n²) over one chat); keep the last sequence in memory.
- **`workflows`** is the workflow source resolver; rename it to
  `workflow::resolver` to end the `workflow`/`workflows` confusion.
- **Compression of conversations left open by a crash.** Step 2 removed the
  unused startup sweep; nothing now compresses such a file. Decide whether
  the runner should compress open attempts of a finished execution.

## Security findings (need specialist review)

An automated security review flagged two issues in code that existed before
the split (the files only moved). They are not verified; a specialist must
assess them before any decision.

1. **SSRF: incomplete internal-host check** —
   `crates/narsor-exec/src/registry/remote_policy.rs`. For a DNS host name
   only `localhost` and `*.localhost` are refused, so a name that resolves
   to an internal address passes, and the address can change between the
   check and git's own lookup (DNS rebinding). Suggested direction: resolve
   the name, refuse it when any address is internal, and pin the checked
   address for the clone (for example `http.curloptResolve`) or clone
   through an egress proxy with the same policy.
2. **Fail-open sandbox settings in the GitHub Copilot harness** —
   `crates/narsor-exec/src/engine/actions/harness/github_copilot.rs`.
   `sandbox(request)?` is computed and discarded, and the SDK config carries
   only the prompt, so `workspace_access` and `approval_policy` have no
   effect for this harness. Suggested direction: pass the values to the SDK,
   and refuse to run when the SDK cannot enforce a requested restriction.
