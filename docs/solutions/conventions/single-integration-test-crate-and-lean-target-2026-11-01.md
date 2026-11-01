---
title: "One integration test crate (tests/integration/main.rs) and lean debug info keep target/ small"
date: 2026-11-01
category: conventions
module: tests, Cargo.toml
problem_type: convention
component: testing_framework
severity: medium
applies_when:
  - "Adding a new integration test file under tests/"
  - "target/ grows large after many cargo test builds"
  - "A test needs process-global state, a fixed port, or a spawned built binary"
tags:
  - cargo
  - integration-tests
  - target-size
  - debuginfo
  - test-layout
---

# One integration test crate and lean debug info keep target/ small

## Problem
Every file directly in `tests/` is its own test crate. Each one links the
whole workspace plus full debug info for every dependency, so ~15 files
(and `target/`) became huge. Many of them also carried infra-dependent tests.

## Convention
- Integration tests of the `narsor` package are a single crate:
  `tests/integration/main.rs`. Each sibling file is a module and **must be
  declared there with `mod`**. Cargo does not auto-discover it, so a file
  without `mod` silently never runs.
- Shared helpers are `*_support` modules, used as `crate::<name>_support`
  (not `mod x_support;` inside each test file).
- All modules run concurrently in one process. Do not use `set_var`,
  `set_current_dir`, or fixed ports. Use temp dirs and `:0` ports.
- Tests that spawn built binaries (`CARGO_BIN_EXE_*`) or `cargo` do not belong
  in this crate. Tests that need real processes or infra were removed or kept
  `#[ignore]`, not guarded.
- `Cargo.toml` sets `[profile.dev.package."*"] debug = "line-tables-only"`
  (workspace crates keep full debug info) and `[profile.release] strip = "symbols"`.
- Clean stale artifacts with `cargo sweep --time 14`.

## Why
Linking cost and debuginfo scale with the number of test crates; merging
them removed ~14k lines of duplicated per-crate setup. Line tables still give
usable backtraces for dependency frames.

## References
- docs/dev/building-and-testing.md ("Test layout", "Keeping `target/` small")
- commit 8d7aaf6
