# Workspace package boundary review

## Actionable Findings

None unresolved. Three review findings were resolved during the work: legacy workflow capability compatibility, removal of orphaned API server sources, and explicit rejection of `narsor-cli daemon start`. Details are in `actionable-findings.json`.

## Coverage

Reviewed correctness, security, API contracts, maintainability, testing, and reliability. Correctness and security returned no remaining findings. API compatibility and the missing CLI start assertion were fixed and verified. The maintainability finding was fixed by deleting the uncompiled client-tree copies of the API server modules.

The independent cross-model peer job (Claude Opus 5 high) reached a terminal `done` state but emitted no normalized result artifact. Cross-model review coverage is therefore unavailable; the attempt and its job ID are recorded in `metadata.json`.

Advisory testing gaps:

- Package-scoped process tests locate sibling executables in the shared Cargo target profile and may reuse an older artifact if invoked alone. The full workspace build and serial test run passed.
- There is no installed-style integration test that exercises `narsor daemon stop` resolving and launching the adjacent `narsor-server` binary.

Plan coverage: R1–R11 and implementation units U1–U7 are implemented. The workspace check, normal serial workspace test suite, clippy, all four package-specific binary builds, formatting check, diff check, metadata check, and client dependency-tree check passed.

The exact include-ignored suite fails at three tests already marked ignored in the branch baseline: `conductor_status_prints_what_the_conductor_is_doing`, `recover_restarts_a_failed_step_shown_as_a_problem`, and `the_cli_and_the_api_share_the_conductors_chat`. Their ignore reasons say scheduler-admitted runner executions do not yet support conductor/recovery operations and refer to the unified task scheduler follow-up. The expanded suite passed when those three known cases and the live-cluster test were skipped. The live-cluster test requires a cluster environment.

## Verdict

Complete for the workspace package-boundary plan. No unresolved correctness, security, API-contract, or maintainability findings block the refactor. The exact include-ignored command exposes three scheduler-follow-up tests that were already intentionally ignored in the baseline; the normal workspace suite and the rest of the ignored process/lifecycle coverage pass. The remaining test gaps are documented as advisory.
