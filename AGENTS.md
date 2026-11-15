# AGENTS.md

Instructions for AI coding agents (Claude Code, Codex, OpenCode, Copilot)
working in this repository. `CLAUDE.md` only includes this file.

## Language

Write everything in this repository in **English**: code, identifiers,
comments, documentation, tests, commit messages, PR/MR titles and
descriptions, and new files under `docs/` (plans, brainstorms, solutions,
reviews). Existing Polish documents under `docs/` are history — do not
translate them, but write new content and edits in English.

## Project

Narsor is a software factory: it runs YAML-defined workflows (states,
actions, decisions, transitions) that orchestrate AI agents and people.
Rust 2021, Cargo workspace.

- `src/` — the `narsor` (TUI) and `narsor-cli` binaries, client library.
- `crates/` — workspace crates: `narsor-engine`, `narsor-controller`,
  `narsor-controller-runtime`, `narsor-runner`, `narsor-server`,
  `narsor-web`, `narsor-protocol`, `narsor-shared`, `narsor-api-client`,
  `narsor-observer-worker`.
- `tests/` — integration tests. `deploy/` — deployment manifests.
  `dist/` — packaging scripts.

Start with `docs/dev/architecture.md`, then `docs/dev/domain-model.md` and
`docs/dev/workflow-schema.md`.

## Build and test

```bash
cargo build --workspace --bins
cargo test --workspace                        # what CI runs (with --locked)
cargo test --workspace -- --include-ignored   # also process lifecycle tests
```

`#[ignore]`d tests start real processes, call the real `claude` binary
(costs money) or need a live Kubernetes cluster — run them only when the
change needs them. Binaries must sit next to each other; see
`docs/dev/building-and-testing.md`.

## Working rules

- Run `cargo test --workspace` before you report a change as done.
- Keep changes in scope; follow the patterns of the crate you touch.
- Do not commit or push unless asked. Commit format:
  `<gitmoji> [NARSOR] <type>(<scope>): <summary>`.
- Never commit `.compound-engineering/config.local.yaml` or other local
  config.

## Knowledge store

- `docs/solutions/` — documented solutions to past problems (bugs,
  architecture and design patterns, conventions), organized by category
  with YAML frontmatter (`module`, `tags`, `problem_type`, `applies_when`).
  Relevant when implementing or debugging in documented areas.
- `docs/brainstorms/`, `docs/plans/` — requirements and implementation
  plans. `docs/code-reviews/`, `docs/residual-review-findings/`,
  `docs/follow-ups/` — review output and deferred work.
- `docs/operations/`, `docs/user/` — operator and user documentation.
