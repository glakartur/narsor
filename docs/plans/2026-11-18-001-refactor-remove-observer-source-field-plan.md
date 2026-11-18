---
title: "refactor: Remove the observer source field"
type: refactor
status: completed
date: 2026-11-18
---

# refactor: Remove the observer source field

## Summary

Remove the `source` field from observers everywhere it appears: the engine
model and its validation, the HTTP create request and the observer view, the
script stdin contract, the CLI (`--source` and text output), the TUI and web
UI forms and detail views, the user docs, the example script headers and the
tests. The field is a free-text label that no observer behaviour reads.

---

## Problem Frame

An observer requires `--source TEXT` at creation. Narsor only checks its
format (1–256 characters, no control characters), stores it, shows it, and
passes it to the script as `observer.source`. Nothing selects an adapter,
URL, credential or schedule from it — the script reads the external system
using `condition` and `secrets`. The field is a required input that carries
no meaning for the system, and users confuse it with real configuration
(asked as "what is `source` in an observer definition?").

---

## Requirements

- R1. Creating an observer no longer takes a source: the CLI has no
  `--source` option, the TUI and web UI create forms have no source field,
  and the HTTP create request has no `source` field.
- R2. The HTTP create request keeps rejecting unknown fields, so a request
  that still sends `source` fails with a validation error (hard removal; no
  accept-and-ignore period).
- R3. Observer views (HTTP `ObserverView`, `observer show`/`list` text, TUI
  queue detail, web UI queue detail) no longer show a source.
- R4. The script stdin document no longer has `observer.source`;
  `contract_version` stays `2`, so already-approved scripts keep running.
- R5. Observer records persisted before this change load without error and
  keep every other field; the stale `source` key disappears on the next
  write. No record version bump, no migration.
- R6. User docs and example script headers describe observer creation and the
  script input without a source.

---

## Scope Boundaries

- Not renaming or removing other observer fields (`condition`, `queue`,
  `secrets`, …).
- Not touching unrelated `source` identifiers: token sources in
  `narsor-api-client`, `source: workflow` variables, repository sources,
  conversation event `source`, `FAKE_SOURCE_DIR` in the example scripts
  (that variable names the fake external system, not the field).
- Not rewriting existing Polish documents under `docs/` or historical
  plans/brainstorms that mention `--source`.

---

## Key Technical Decisions

- **Hard removal at the API boundary (user decision).** The workspace is at
  `0.1.0` and clients ship with the server. `CreateObserverRequest` keeps
  `deny_unknown_fields`; an old client that sends `source` gets a validation
  error. This avoids a deprecated field that would need a second removal.
- **Script contract stays at version 2 (user decision).** The scripts are
  told to stop when `contract_version` differs from 2; bumping it would stop
  every approved script until each is re-saved and re-approved. No example
  script reads `observer.source`. A custom script that does read it now gets
  `null`/missing — note this in the user docs.
- **Rely on serde's default for persisted records.** `ObserverConfig` and
  `ObserverRecord` do not use `deny_unknown_fields`, so a record written with
  `source` still deserializes. Keep it that way, and add a store test that
  proves it, so a later `deny_unknown_fields` cannot silently break old data
  directories.
- **`ObserverConfig::new` loses its source parameter.** Every caller
  (owner service, tests) changes in the same unit; there is no reason to
  keep a dead argument.

---

## Implementation Units

### U1. Engine model, protocol and script contract

**Goal:** Remove `source` from the domain and wire types and from the script
input.

**Requirements:** R1 (API part), R2, R3 (API part), R4, R5

**Dependencies:** none

**Files:**
- `crates/narsor-engine/src/observers/model.rs` — drop `ObserverConfig.source`,
  the constructor parameter, the source check in `validate`, and
  `MAX_SOURCE_LEN`.
- `crates/narsor-engine/src/observers/contract.rs` — drop
  `ScriptObserver.source`; update the module doc of the stdin document.
- `crates/narsor-engine/src/observers/mod.rs` — re-exports, if they name the
  constant.
- `crates/narsor-protocol/src/controller/observer.rs` — drop
  `CreateObserverRequest.source` and `ObserverView.source`.
- `crates/narsor-engine/src/controller_protocol/service/observers/owner.rs` —
  create path and view building.
- `crates/narsor-engine/src/controller_protocol/service/observers/schedule.rs`
  — building `ScriptObserver`.
- Tests: `crates/narsor-protocol/src/controller/tests.rs`,
  `crates/narsor-server/src/controller_protocol/observer_http.rs` (test
  module), `tests/integration/observers_store.rs`.
- `ObserverConfig::new` callers that pass a source literal and must drop it:
  `crates/narsor-engine/src/controller_protocol/service/observers/tests.rs`,
  `tests/integration/observers_effects.rs`,
  `tests/integration/observers_schedule.rs`,
  `tests/integration/observers_decide.rs`,
  `tests/integration/observers_worker.rs`,
  `tests/integration/observers_queue_entries.rs`.

**Approach:** Remove the field and follow the compiler errors through the
engine and server. Do not add `#[serde(skip)]` or a deprecated alias.

**Patterns to follow:** existing round-trip tests in
`crates/narsor-protocol/src/controller/tests.rs`; legacy-record load tests in
`tests/integration/observers_store.rs`.

**Test scenarios:**
- A create request JSON that contains `"source"` fails to deserialize
  (unknown field), and the HTTP create endpoint answers it with a validation
  error, not a 500.
- A create request without `source` creates a draft observer whose view has
  no `source` key.
- A record file written in the old shape (with `"source": "jira"`) loads from
  the store with name, queue, mode, condition and secrets intact; after a
  config change is saved, the file no longer has `source`.
- The `ScriptInput` a scheduled run hands to the script serializes without
  `observer.source` and with `contract_version: 2`.

**Verification:** the workspace builds; no `source` remains on observer
types; the tests above pass.

### U2. CLI

**Goal:** Remove `--source` and the source line from the CLI.

**Requirements:** R1, R3

**Dependencies:** U1

**Files:**
- `src/cli/observer.rs` — create args and request building.
- `src/cli/observer_text.rs` — the `Source:` line in `observer show` and the
  hint text that prints `observer create NAME --source TEXT --queue …`.
- `src/cli/mod.rs`, `src/cli/remote.rs` — only if they pass the field.
- Tests: `tests/integration/cli_observer.rs`,
  `tests/integration/cli_control_text.rs` (if it asserts the hint).

**Approach:** Delete the clap argument; clap then rejects `--source` with its
usual unknown-argument error (exit code 2).

**Test scenarios:**
- `observer create NAME --queue Q --mode on_episodes --cron …` succeeds
  without `--source` and `observer show` prints no `Source:` line.
- `observer create … --source x` exits with code 2 (unknown argument).

**Verification:** the CLI tests pass with the updated invocations.

### U3. TUI and web UI

**Goal:** Remove the source field from the create forms and detail views.

**Requirements:** R1, R3

**Dependencies:** U1

**Files:**
- `src/tui/queues/forms.rs` — the `SOURCE` field constant, the form field
  list, and request building.
- `src/tui/queues/detail.rs` — the `source:` part of the state line.
- `crates/narsor-web/ui/js/model.js` — form defaults, the required-source
  check and the create request body.
- `crates/narsor-web/ui/js/ui/queues.js` — the `źródło` field in the create
  form and the `źródło` row in the detail view.
- Tests: `tests/integration/tui_queues.rs`, `tests/integration/web_ui.rs`.

**Approach:** Remove the field; keep the order of the remaining form fields.
The web UI has no JS unit tests; `web_ui.rs` covers the HTTP side.

**Test scenarios:**
- The TUI create form submits a valid observer without a source field, and
  the queue detail line shows state and mode only.
- The web UI integration test creates an observer with a body that has no
  `source`.

**Verification:** TUI and web UI tests pass; a manual look at the web UI
create form and queue detail shows no source.

### U4. Docs and example scripts

**Goal:** Describe observers without a source.

**Requirements:** R4, R6

**Dependencies:** U1, U2

**Files:**
- `docs/user/observers.md` — `observer create` examples, the stdin example
  and the field table (`observer` row: id and name), the fake-source test
  example on stdin, and one sentence that `observer.source` is gone while the
  contract version stays 2.
- `docs/user/cli.md` — the `observer create` synopsis and example.
- `workflows/observers/examples/jira-new-issues.sh`,
  `workflows/observers/examples/github-pr-review.sh`,
  `workflows/observers/examples/gitlab-mr-review.sh` — the `observer create`
  command in each header.
- Test: `tests/integration/observers_examples.rs` — the stdin it feeds the
  examples.

**Approach:** Edit only the lines about the field. Write in English.

**Test scenarios:**
- The example scripts still pass their fake-source tests with an input that
  has no `observer.source`.

**Verification:** no `--source` or `"source"` (observer sense) remains in
`docs/user/` and the example headers; `observers_examples` passes.

---

## Risks

| Risk | Mitigation |
|---|---|
| A user's custom approved script reads `observer.source` and now gets nothing. | No bundled script reads it; the user docs state the removal. The contract version stays 2 by decision. |
| An older CLI or browser tab sends `source` to a new server. | Accepted: hard removal. The server answers with a validation error, not a crash. Update clients with the server. |
| A future `deny_unknown_fields` on the record breaks old data directories. | The U1 store test loads an old-shape record. |
