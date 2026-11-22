---
title: "fix: init config file shows env variables and writes block YAML"
type: fix
status: completed
date: 2026-11-22
origin: docs/brainstorms/2026-11-22-init-config-env-comments-and-block-yaml-requirements.md
---

# fix: init config file shows env variables and writes block YAML

## Overview

`narsor-cli init` (`src/cli/init.rs`) writes the configuration file. Two changes
to that output:

1. The comment of each setting names its environment variable
   (`Env: NARSOR_CONTROLLER__LOCAL__HOME`) instead of the former flat key
   (`Formerly \`local_home\`.`).
2. Every written list and map is in block YAML with indented items, instead
   of one-line flow style (`{ claude: [...] }`).

The parser, the env naming scheme and the legacy-name warning do not change.

---

## Problem Frame

The env mapping is already correct (`SettingKey::env_var` in
`crates/narsor-shared/src/config/settings.rs`), but the written file shows
only the old flat key next to each setting. This makes operators think that
the mapping is flat. Compound values come out in flow style, which looks
like JSON and is hard to extend by hand
(see origin: docs/brainstorms/2026-11-22-init-config-env-comments-and-block-yaml-requirements.md).

---

## Requirements Trace

- R1, R2. Comment names `Env: NARSOR_<SEGMENTS>` from `SettingKey::env_var`
  for written settings and commented defaults → U1
- R3. No "Formerly" in the written file; legacy warning and docs table keep
  former names → U1
- R4, R5, R6. Block YAML for lists and maps at any depth, scalars unchanged,
  empty values short, tokens round-trip in order → U2
- R7. `agent_args` comment shows a commented block example; re-run behavior
  unchanged → U2
- R8. Docs show `agent_args` in block YAML and mention the `Env:` comment → U3

---

## Scope Boundaries

- No change to `SettingKey::env_var`, `ENV_PREFIX` or the `__` separator.
- No change to `crates/narsor-shared/src/config/file.rs` parsing; flow YAML
  stays valid input, also in `NARSOR_RUNNER__AGENT_ARGS`.
- CLI-flag variables with one underscore (`NARSOR_CONTROL_PLANE`,
  `NARSOR_TOKEN_FILE`, …) are out of scope.
- `narsor config show/check` output does not change.
- `SettingKey::former_key`, `crates/narsor-shared/src/config/legacy.rs` and
  the migration table in `docs/user/configuration.md` stay as they are.

---

## Context & Research

### Relevant Code and Patterns

- `src/cli/init.rs`:
  - `render_config` → `entry_of` → `render_group` build the file; each
    `Entry` holds `EntryLine::Written(String)` or `EntryLine::Default(&str)`.
  - `comment_of` builds the description plus `Formerly \`…\`` from
    `SettingKey::former_key`.
  - `inline_yaml` renders a `serde_yaml::Value` in flow style; it is the only
    renderer for written values.
  - `OPTIONAL_SETTINGS` holds the `AgentArgs` description with a flow example
    (`e.g. { claude: [--dangerously-skip-permissions] }`) and default `{}`.
  - `validate_config` renders the file and parses it back with `Scope::ALL`,
    so every written file is already round-trip checked at write time.
  - The file header already explains the `NARSOR_<SECTION>__<KEY>` scheme.
- Tests in the `tests` module of `src/cli/init.rs`:
  - `local_install_keeps_the_layout_and_every_default_can_be_switched_on`
    uses `uncomment_all`/`is_key_line`; `is_key_line` accepts only lowercase
    keys, so a `# Env: …` line is not taken as a key.
  - `server_role_writes_network_settings_uncommented` asserts that the line
    above `issuer:` contains `` `oidc_issuer` `` — this must change.
  - `rerun_keeps_the_configuration_and_an_existing_task_image` covers re-run.
- `serde_yaml` 0.9 block output does not indent a sequence under its mapping
  key (`claude:\n- a`). That is valid YAML but does not match the indented
  style in the origin examples and in `deploy/`, so the block renderer is
  written by hand, in the same recursive style as `inline_yaml`.

### Institutional Learnings

- No entry in `docs/solutions/` covers init rendering or config output.

---

## Key Technical Decisions

- **Env comment on its own line** under the description
  (`# <description>` then `# Env: NARSOR_…`), for written settings and
  commented defaults alike. It reads cleanly and keeps the description short.
  `Env` starts with a capital letter, so it is never mistaken for a key when
  a user removes `# ` from key lines.
- **Name taken from `SettingKey::env_var`** — the same function the resolver
  uses, so the comment and the lookup cannot drift (R2).
- **Hand-written block renderer**: a mapping entry with a compound value
  writes `key:` and its content one indent level deeper; a sequence writes
  `- item` lines one level under its key; scalars reuse the existing scalar
  path (`serde_yaml::to_string` of the scalar, trimmed), which adds quotes
  only where YAML needs them (R6). Empty sequence/mapping stay `[]` / `{}`
  on the key line (R5).
- **Re-run stays as it is**: `run_with` always builds the config from an
  empty mapping; when the file exists it writes `config.yaml.new` and leaves
  the original untouched (`rerun_keeps_the_configuration_and_an_existing_task_image`
  asserts this). No merge of the existing file (see origin R7).
- **`agent_args` example moves into a commented block**: init never writes
  `runner.agent_args`, so the comment is the only place an operator copies
  the shape from. The description drops `{ claude: [...] }` and is followed
  by commented example lines in block style (`#   claude:`,
  `#     - --dangerously-skip-permissions`). The leading spaces after `# `
  keep `is_key_line` from treating them as keys, so `uncomment_all` still
  switches on only the `{}` default and the file still parses.

---

## Implementation Units

### U1. Env-variable comment instead of the former key

**Goal:** Each setting's comment in the written file names its env variable.

**Requirements:** R1, R2, R3

**Dependencies:** None

**Files:**
- Modify: `src/cli/init.rs` (`comment_of`, `render_group`)
- Test: `src/cli/init.rs` (`tests` module)

**Approach:**
- `comment_of` returns only the description; drop the `former_key` branch.
- `render_group` writes a second comment line `# Env: <setting.env_var()>`
  after the description, for both `Written` and `Default` entries, at the
  same indent.
- Leave `SettingKey::former_key` in place; `legacy.rs` still reads it.

**Patterns to follow:** existing comment emission in `render_group`.

**Test scenarios:**
- Covers AE1. Happy path: a fresh local init file has, directly above the
  `# home: host` line, a line `# Env: NARSOR_CONTROLLER__LOCAL__HOME`, and no
  line contains `Formerly`.
- Happy path: in the server-role file, the line above `issuer:` is
  `# Env: NARSOR_SERVER__OIDC__ISSUER` (replaces the `` `oidc_issuer` ``
  assertion in `server_role_writes_network_settings_uncommented`).
- Edge case: every setting in the file has exactly one `# Env:` line, and
  `uncomment_all` of the file still parses in every scope (existing test must
  keep passing).

**Verification:** the written file shows an `Env:` line for every setting and
no former names; existing init tests pass.

---

### U2. Block YAML for written lists and maps

**Goal:** Compound values in the written file are in indented block style.

**Requirements:** R4, R5, R6, R7

**Dependencies:** None (can land before or after U1)

**Files:**
- Modify: `src/cli/init.rs` (`entry_of`, `EntryLine`, `render_group`,
  `inline_yaml` → block renderer, `OPTIONAL_SETTINGS` `AgentArgs` text)
- Test: `src/cli/init.rs` (`tests` module)

**Approach:**
- A written value is either a one-line scalar (as today, on the key line) or
  a block: `key:` on its own line, then the rendered lines indented one level
  under the key's indent.
- Block renderer, recursive over `serde_yaml::Value`:
  - sequence → one `- <scalar>` line per item; nested compound items follow
    YAML block rules;
  - mapping → `k: <scalar>` or `k:` plus nested block;
  - empty sequence / mapping → `[]` / `{}` kept on the key line;
  - scalars → current scalar rendering (quotes only where needed).
- Keep `validate_config` as the write-time round-trip guard.
- Update the `AgentArgs` description: no flow example; state that each list
  item is one argument, and render a commented block example under it
  (e.g. `claude:` with `--dangerously-skip-permissions`, `--extra_param`,
  `extra param value`).

**Technical design:** *(directional guidance, not implementation
specification)*

```text
runner:
  # Extra arguments by harness; one list item is one argument. ...
  # Env: NARSOR_RUNNER__AGENT_ARGS
  agent_args:
    claude:
      - --dangerously-skip-permissions
      - --extra_param
      - extra param value
```

**Patterns to follow:** recursive shape of `inline_yaml`; indent handling in
`render_group` (`"  ".repeat(depth)`).

**Test scenarios:**
- Covers AE2. Happy path: a config with `runner.agent_args.claude` =
  `--dangerously-skip-permissions`, `--extra_param`, `extra param value`
  renders the indented block from the design above, and parsing the text
  back gives the same three tokens in the same order.
- Edge case: tokens that need quoting in YAML (a value with `: `, a leading
  `#`, an empty string) survive the round trip unchanged — one table-driven
  test with the `agent_args` round trip.
- Covers AE3. Integration: server-role init with trusted proxies
  `127.0.0.1` and `10.0.0.12` writes both as block items under
  `trusted_proxies:` and the parsed value equals the input (extend
  `server_role_writes_network_settings_uncommented`).
- Covers AE3. Happy path: the written file shows the `agent_args` example as
  commented block lines, has no `{ claude:` text, and
  `uncomment_all` of the file still parses in every scope.
- Edge case: an empty list setting renders as `key: []` and parses as empty.

**Verification:** no written value in a generated file uses flow brackets
except an empty `[]` / `{}`; round-trip tests pass.

---

### U3. Documentation

**Goal:** Docs match the new written file.

**Requirements:** R8

**Dependencies:** U1, U2

**Files:**
- Modify: `docs/user/configuration.md`

**Approach:**
- Full example file: `runner.agent_args.claude` as an indented block list.
- Show `runner.agent_args` in block YAML both in the file example and as the
  value of `NARSOR_RUNNER__AGENT_ARGS` (a multi-line YAML value), replacing
  `{claude: [--effort, high]}` in the settings table; keep the note that the
  env value is YAML text and accepts block YAML.
- "Environment variables" paragraph: say that the file from `narsor-cli init`
  names each setting's variable in an `Env:` comment.
- Do not change the old-and-new-names table.

**Test expectation:** none — documentation only.

**Verification:** no flow-style `agent_args` example remains in
`docs/user/configuration.md`.

---

## System-Wide Impact

- **Affected parties:** operators who run `narsor-cli init`. A re-run still
  leaves the existing file untouched and writes `config.yaml.new`.
- **Unchanged invariants:** env naming, parser input formats, legacy-name
  startup warning, `narsor config` output, K8s `NARSOR_RUNNER__AGENT_ARGS`
  forwarding.
- **Integration coverage:** `tests/integration/config_file.rs` and
  `tests/integration/config_resolve.rs` read files, not init output; they
  should pass unchanged.

---

## Risks & Dependencies

| Risk | Mitigation |
|------|------------|
| Block renderer emits YAML that parses differently (quoting, indent) | `validate_config` parses every written file; round-trip test with tricky tokens |
| A test or doc greps for `Formerly` / flow text | Search `src`, `tests`, `docs` during U1/U3; only the init test was found |

---

## Sources & References

- **Origin document:** [docs/brainstorms/2026-11-22-init-config-env-comments-and-block-yaml-requirements.md](../brainstorms/2026-11-22-init-config-env-comments-and-block-yaml-requirements.md)
- Related code: `src/cli/init.rs`, `crates/narsor-shared/src/config/settings.rs` (`SettingKey::env_var`, `former_key`)
- Related plan: `docs/plans/2026-11-20-001-refactor-hierarchical-configuration-file-plan.md`
