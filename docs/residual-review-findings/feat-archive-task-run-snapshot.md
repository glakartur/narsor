# Residual Review Findings — feat/archive-task-run-snapshot

Source: `ce-code-review` autofix run `20261120-222121-9cc26f4b` on HEAD `b501137`
(base `09906b7`), plan `docs/plans/2026-11-20-002-feat-archive-task-run-snapshot-plan.md`.
Verdict: Ready with fixes. No safe automatic fixes were applied.

## Residual Review Findings

- **P1** `crates/narsor-engine/src/controller_protocol/service/tasks/actions.rs:646` — Tasks with no execution (never ran, queued by mistake, provisioning failed) can no longer be archived; a task that always fails provisioning holds its slot. [#118](https://github.com/glakartur/narsor/issues/118)
- **P2** `crates/narsor-engine/src/controller_protocol/service/tasks/view.rs:166` — Stale terminal snapshot of a previous launch offers archive on a resumed task. [#119](https://github.com/glakartur/narsor/issues/119)
- **P2** `src/tui/task_app/history.rs:347` — TUI history: archived run step rows cannot scroll. [#120](https://github.com/glakartur/narsor/issues/120)
- **P3** `crates/narsor-engine/src/controller_protocol/service/tasks/commands.rs:78` — Public `apply_command` bypasses the owner archive offer. [#121](https://github.com/glakartur/narsor/issues/121)

## Agent-native gap (not filed)

- `narsor-cli task show --json` and `task steps` carry no per-step harness, model, effort, usage or cost for archived tasks (`src/cli/remote.rs:1075`, `:1278`). The API timeline read already serves the data.
