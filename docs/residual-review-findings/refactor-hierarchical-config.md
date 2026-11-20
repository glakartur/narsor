# Residual review findings: refactor/hierarchical-config

Source: ce-code-review run 20261120-014801-368ae8b7 (mode:autofix) on branch
`refactor/hierarchical-config`, plan
`docs/plans/2026-11-20-001-refactor-hierarchical-configuration-file-plan.md`.

## Residual Review Findings

- P2 `src/cli/init.rs:681`: init under `NARSOR_GLOBAL__ROOT` writes relative
  directories without `global.root`.
  [#114](https://github.com/glakartur/narsor/issues/114)
- P2 `crates/narsor-api-client/src/api/discovery.rs:380`: the CLI waits 15 s
  when the auto-launched daemon refuses its configuration.
  [#115](https://github.com/glakartur/narsor/issues/115)
- P2 `crates/narsor-controller/src/local.rs:249`: no test covers how the
  controller selects its configuration source.
  [#116](https://github.com/glakartur/narsor/issues/116)
