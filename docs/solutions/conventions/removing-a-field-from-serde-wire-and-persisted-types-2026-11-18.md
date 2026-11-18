---
title: "Removing a field from serde wire and persisted types"
date: 2026-11-18
category: conventions
module: narsor-protocol, narsor-engine/observers, narsor-web, src/cli, src/tui
problem_type: convention
component: tooling
severity: medium
applies_when:
  - "A field is removed from a protocol request, view or response type"
  - "A field is removed from a type that is stored on disk (records, configs)"
  - "A field is removed from a script or runner contract that has a version number"
  - "A change touches crates/narsor-web/ui/js"
tags:
  - serde
  - deny-unknown-fields
  - wire-compat
  - persisted-records
  - version-skew
  - contract-version
  - web-ui
  - node-tests
---

# Removing a field from serde wire and persisted types

## Context

The observer `source` field was removed from every layer at once: the
engine model, `CreateObserverRequest`, `ObserverView`, the script stdin
document, the CLI, the TUI, the web UI, the docs and the tests (PR
glakartur/narsor#112, plan
`docs/plans/2026-11-18-001-refactor-remove-observer-source-field-plan.md`).
The compiler finds every Rust use of the field. It does not tell you how
each serde type behaves for data and clients of the old version. Each kind
of type fails in a different way, and one part of the repository (the web
UI Node tests) is not checked by `cargo test`.

## Guidance

Before you remove the field, classify each type that holds it. Each kind
has a different effect on old data and old clients:

| Kind of type | serde attribute | Effect of the removal | What to do |
|---|---|---|---|
| Request that the server reads (`CreateObserverRequest`) | `#[serde(deny_unknown_fields)]` | An old client that still sends the field gets `400 invalid_request` (`unknown field ...`). | Keep `deny_unknown_fields` for a hard removal. Add a test that sends the old field and expects the refusal. |
| View or response that the client reads (`ObserverView`) | no `deny_unknown_fields` | A new client reads an old server without problems. An **old** client that declared the field as a required `String` cannot read a new server. The whole `Vec<ObserverView>` fails, so list, show and queue views break. | Ship the server and the clients together. Add an update-order section to `docs/operations/updates.md`. |
| Record stored on disk (`ObserverConfig`, `ObserverRecord`) | no `deny_unknown_fields` | Old records load, and serde ignores the stale key. The next save removes it. No migration and no `record_version` bump are necessary. | Add a store test that loads an old-shape record and checks that the saved file has no key. This test stops a later `deny_unknown_fields` from breaking old data directories without notice. |
| Versioned script contract (`ScriptInput`, `contract_version`) | — | A script that reads the field gets `null` and no error. | Decide whether to bump the version. A bump stops every approved script until it is saved and approved again. Here the version stayed `2`, and the user docs say that `observer.source` is gone. |

Then remove the field and follow the compiler errors. Do not keep the
field with `#[serde(skip)]` or a deprecated alias: that makes a second
removal necessary later.

After the Rust build is clean, search the parts that the compiler does not
check:

```sh
# JS, docs, shell and JSON fixtures in tests
grep -rn '"source"\|\.source\b\|--source' crates/narsor-web docs/user workflows tests
# The web UI unit tests: cargo test and CI do not run them
node --test crates/narsor-web/ui-tests/*.mjs
```

## Why This Matters

- `cargo test --workspace` passed while
  `crates/narsor-web/ui-tests/queues.test.mjs` still expected `source` in
  the create form. Only a code review found it. CI does not run the Node UI
  tests, so a JS regression in `crates/narsor-web/ui` stays green.
- The view-type failure only appears with **mixed versions**. All tests run
  one build against itself, so no test can show that an old CLI breaks on a
  new server. Only the update notes can protect the operator.
- The persisted-record compatibility depends on an attribute that is
  *absent*. Without a test, someone can add `deny_unknown_fields` to a
  record type later, and every data directory with old records stops
  loading.

## When to Apply

- When you remove or rename a field on a type in `narsor-protocol`, or on
  any type that is stored on disk or sent to a script.
- When you edit `crates/narsor-web/ui/js`: run the Node tests, because
  `cargo test` does not.

## Examples

A test that keeps the hard removal (protocol unit test):

```rust
let old_client = serde_json::json!({"name": "jira-new", "source": "jira"});
let error = serde_json::from_value::<super::CreateObserverRequest>(old_client).unwrap_err();
assert!(error.to_string().contains("unknown field `source`"), "{error}");
```

A store test that proves an old record loads and loses the key on save:

```rust
let lock = store.lock(&jira).unwrap();
store.save(&lock, &mut saved, now()).unwrap();
let raw: serde_json::Value = serde_json::from_slice(
    &fs::read(root.paths().observer_record_file(jira.as_str())).unwrap(),
).unwrap();
// The removed `source` label of an older record goes with the next save.
assert!(raw["config"].get("source").is_none());
```

A removed CLI option is a plain clap error (exit code `2`). Test it with
`Cli::try_parse_from`, because the test helper panics on parse errors:

```rust
let error = Cli::try_parse_from(["narsor-cli", "observer", "create", "gitlab-mr", "--source", "gitlab"])
    .unwrap_err();
assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
assert_eq!(error.exit_code(), 2);
```

## Related

- `docs/operations/updates.md`, section "Update that removes the observer
  source field": the update-order note for this change.
- `docs/solutions/design-patterns/output-contract-once-per-session-runtime-marker-2026-11-12.md`:
  a different serde persistence question (a runtime-only field that must
  stay out of stored definitions).

Sources: PR glakartur/narsor#112, commit 4b79dd3.
