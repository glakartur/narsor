# Contributing to Narsor

## Development setup

- Rust (2021 edition) with `cargo`.
- `git` on `PATH` (a runtime prerequisite; Narsor shells out to it).
- Optionally an agent harness (`claude`, `codex`, `opencode`) for trying agent
  workflows by hand.

See [Building and testing](docs/dev/building-and-testing.md) for the details,
including why binaries must sit next to each other.

## Build and test

```bash
cargo build --workspace --bins
cargo test --workspace
cargo test --workspace -- --include-ignored  # also process lifecycle tests
cargo build --workspace --bins --release
```

### Ignored tests

Some tests are `#[ignore]`d on purpose: they start real processes
(`narsor-controller`), drive the real `claude` binary (costs money, needs
credentials), need a live Kubernetes cluster (`NARSOR_K8S_SMOKE_*`), or are
waiting for the unified task scheduler follow-up (four cases in
`tests/cli_smoke.rs`). Run them with `-- --ignored` or `--include-ignored`;
the scheduler-related cases are expected to fail for now.

## Architecture

Start with [`docs/dev/architecture.md`](docs/dev/architecture.md), then the
[domain model](docs/dev/domain-model.md) and the
[workflow schema](docs/dev/workflow-schema.md). Plans, brainstorms and
solution notes under `docs/` are Polish working artifacts and are kept as
history.

## Release checklist

1. Tag and publish the release (the `release.yml` workflow builds the assets,
   including `install.sh`).
2. Bump the pinned tag in the install one-liners in `README.md` (all
   occurrences) and in `docs/operations/installation.md` to the new `vX.Y.Z`.
   All occurrences must name the same tag.
3. Confirm the new URL answers with HTTP 200:
   `curl -fsSL -o /dev/null -w '%{http_code}' https://github.com/glakartur/narsor/releases/download/vX.Y.Z/install.sh`.
