---
date: 2026-11-22
topic: init-config-env-comments-and-block-yaml
---

# `narsor-cli init` config file: env-variable comments and block YAML

## Summary

The configuration file that `narsor-cli init` writes names, for each setting,
the environment variable that sets it (`Env: NARSOR_CONTROLLER__LOCAL__HOME`)
instead of the old flat key (`Formerly \`local_home\``). The file also writes
every list and map in block YAML, one item per line, instead of one-line flow
style.

---

## Problem Frame

The hierarchical configuration (see
`docs/brainstorms/2026-11-20-hierarchical-configuration-file-requirements.md`)
already maps every setting to an environment variable mechanically:
`NARSOR_` plus the path segments in capitals, joined by `__`. But the file
that `narsor-cli init` writes does not show this mapping. Each setting has a
comment like `Formerly \`local_home\`.`, which names the flat key from before
the migration. An operator who reads the file sees a flat, single-underscore
name next to a nested key and concludes that the env mapping is flat or
wrong. To find the real variable name, they must open
`docs/user/configuration.md`.

The same file writes every compound value on one line (for example
`trusted_proxies: [127.0.0.1]`). `runner.agent_args` is not written by init,
but its comment and the docs show it as
`agent_args: { claude: [--dangerously-skip-permissions] }`, so operators copy
that shape. This looks like JSON, it is hard to extend by hand, and it does not match the
block style of the rest of the file or of the examples in `deploy/`. An
argument that has a value (`--extra_param` followed by `extra param value`)
is especially hard to read and edit in flow style. The parser already accepts
block YAML in the file and in `NARSOR_RUNNER__AGENT_ARGS`, so only the
written output causes the problem.

---

## Requirements

**Setting comments**
- R1. In the file that `narsor-cli init` writes, the comment for each setting
  names the environment variable for that setting, in the form
  `Env: NARSOR_<SEGMENTS>` (for example `Env: NARSOR_CONTROLLER__LOCAL__HOME`).
  This applies to written settings and to commented-out defaults.
- R2. The comment uses the same name that the program reads. It comes from the
  same single source as the variable lookup, so the two cannot disagree.
- R3. The written file no longer mentions the former flat key ("Formerly …").
  The startup warning about un-migrated names and the migration table in
  `docs/user/configuration.md` keep the former names.

**Compound values**
- R4. `narsor-cli init` writes every list and map value in block YAML: each list
  item on its own line, each map entry under its key, nested to any depth.
  This applies to `runner.agent_args` and to all list settings (for example
  `server.tls.trusted_proxies`, `server.local_workspace_roots`,
  `server.cors_allowed_origins`).
- R5. Scalar values keep their current one-line form. An empty list or map
  keeps a short form that the parser reads as empty.
- R6. Each `agent_args` item stays one argv token in the given order, so an
  option and its value are two items (`--extra_param`, `extra param value`).
  Quoting is added only where YAML needs it, and the file reads back to the
  same values.
- R7. The comment of `runner.agent_args` shows its example in block YAML
  (commented lines), not as `{ claude: [...] }`. `narsor-cli init` never
  writes this setting, so the comment is what operators copy from.
  Re-running `narsor-cli init` keeps its current behavior: it does not read or
  change an existing file and writes a fresh `config.yaml.new`.

**Documentation**
- R8. `docs/user/configuration.md` shows `runner.agent_args` in block YAML
  (for the file and for the `NARSOR_RUNNER__AGENT_ARGS` value) instead of
  the flow example `{claude: [--effort, high]}`, and mentions the `Env:`
  comment in the written file.

---

## Acceptance Examples

- AE1. **Covers R1, R3.** Given a fresh `narsor-cli init`, the `controller.local`
  group contains `# Env: NARSOR_CONTROLLER__LOCAL__HOME` above `# home: host`,
  and no line in the file contains `Formerly`.
- AE2. **Covers R4, R6.** Given `agent_args` with a `claude` list of
  `--dangerously-skip-permissions`, `--extra_param`, `extra param value`, the
  file contains:
  ```yaml
  runner:
    agent_args:
      claude:
        - --dangerously-skip-permissions
        - --extra_param
        - extra param value
  ```
  and reading the file back gives the same three tokens in the same order.
- AE3. **Covers R4, R7.** Given `narsor-cli init` for the server role with
  trusted proxies `127.0.0.1` and `10.0.0.12`, the file contains the two
  addresses as block list items under `trusted_proxies:`, and the
  `agent_args` comment shows a commented block example with one argument
  per line.

---

## Success Criteria

- An operator can find the environment variable for any setting by reading
  the file that `narsor-cli init` wrote, without opening the documentation.
- An operator can add a harness argument by adding one line to the file.
- Every value written by `narsor-cli init` reads back to the same effective
  configuration (round-trip).

---

## Scope Boundaries

- The env-variable naming scheme does not change. The `NARSOR_` prefix and
  the `__` level separator stay.
- The parser and the accepted input formats do not change. Flow-style YAML
  stays valid input in the file and in `NARSOR_RUNNER__AGENT_ARGS`.
- CLI-flag environment variables with one underscore (`NARSOR_CONTROL_PLANE`,
  `NARSOR_KUBERNETES_CONFIG`, `NARSOR_CONTROLLER_CREDENTIAL_FILE`,
  `NARSOR_ENROLLMENT_CODE_FILE`, `NARSOR_SERVER`, `NARSOR_TOKEN_FILE`, …) are
  not configuration-file settings. They are out of scope.
- The output of `narsor config` commands (show/check) does not change.
- The legacy startup warning and the migration table stay as they are.
- Re-running `narsor-cli init` does not read, merge or reformat an existing
  file (unchanged behavior).

---

## Key Decisions

- Show the env variable, not both names: the former flat key only helps
  during migration. The startup warning and the docs already cover that
  case, and the old name in the file was the cause of the confusion.
- Block style for all compound values, not only `agent_args`: one consistent
  style in the file, and it matches the YAML in `deploy/`.

---

## Outstanding Questions

### Deferred to Planning

- [Affects R1][Technical] Exact comment layout: a separate `# Env: …` line
  under the description, or the variable appended to the description line.
- [Affects R5][Technical] Exact short form for empty values (`[]` / `{}`).
