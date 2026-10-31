---
title: "docs: split README into user-facing README + docs/user, docs/dev"
type: docs
status: active
date: 2026-10-31
origin: docs/brainstorms/2026-10-31-dokumentacja-podzial-readme-requirements.md
---

# docs: split README into user-facing README + separate docs

## Overview

Rewrite `README.md` (1375 lines) as a short English user page (<=120 lines), move the rest into `docs/user/` and `docs/dev/`, move and translate the three Polish root design docs into `docs/dev/`, fix the install one-liner, add `CONTRIBUTING.md` and `docs/README.md`, and repair all references. No code behaviour change.

## Decisions (resolving origin open questions with its stated assumptions A1-A7)

- **Q1 / install URL:** use tag-pinned asset `https://github.com/glakartur/narsor/releases/download/<tag>/install.sh` (all releases are pre-releases, so `/latest/` 404s; verified `gh release list` shows v0.4.0 as newest, Pre-release). Keep `dist/install.sh` default `latest` unchanged (A7).
- **Keeping the tag current:** the tag is bumped as part of the release procedure (documented in `CONTRIBUTING.md`). Workspace `Cargo.toml` is at `0.1.0` while the newest tag is `v0.4.0`, so a Cargo-version equality guard cannot work. The guard test instead asserts that README/`docs/operations/installation.md` use one single tag of the form `vX.Y.Z` (same in all occurrences) and, offline, matches a tag present in `git tag` when available (skip when no tags, e.g. shallow CI checkout).
- **Q2:** historical `docs/{brainstorms,plans,solutions,...}` stay Polish, untouched.
- **Q3:** English only; Polish originals live in git history (`git mv` then translate in a separate commit so history/blame follow).

## Implementation units

### U1. Verify and fix install URL
- Run `curl -fsSL -o /dev/null -w '%{http_code}' -L <tag-pinned url>` for the newest tag; require 200. Confirm `latest` URL returns 404 (record as rationale).
- Update the five occurrences in `docs/operations/installation.md` (lines ~16, 22, 73, 92, 126) and the README ones to the pinned URL, with `NARSOR_VERSION`/`--version` not needed since the tag is baked in by `release.yml`.
- `dist/install.sh`, `release.yml`, and `tests/install_script.rs` (assert `/releases/latest/download/` for default mode) stay unchanged; only add a note in installation.md on why README pins a tag.
- Add guard test (e.g. `tests/docs_install_url.rs`) per Decisions.

### U2. Move design docs (git mv, then translate)
- `git mv ARCHITECTURE_DECISIONS.md docs/dev/architecture.md`, `DOMAIN_MODEL.md docs/dev/domain-model.md`, `WORKFLOW_SCHEMA.md docs/dev/workflow-schema.md` (commit the move alone first).
- Translate to English faithfully (128 / 442 / 880 lines); keep decision IDs (`D12` etc.), headings' anchors semantics, and the normative status statement. Large files: translate section by section, diff section/heading counts against the original to prove no loss.

### U3. Distribute README content (no content loss)
Map every README section (line refs from current file):

| README section | Destination |
|---|---|
| Prerequisites (21), Building and testing (37) | `docs/dev/building-and-testing.md` (user-visible requirements summarised in README/installation.md) |
| Installing a prebuilt release (70) | `docs/operations/installation.md` (merge; dedupe) + short README install |
| Quick start (116) | README (trim to 3-4 commands) |
| Global options (140), CLI commands (157-519), Exit codes (1053) | `docs/user/cli.md` |
| TUI (520-737) | `docs/user/tui.md` |
| Remote tasks and backends (738) | `docs/user/remote.md`, linking `docs/operations/remote-backends.md` |
| The conductor (780) | `docs/user/conductor.md` |
| AgentAction (830), Sessions, read_only_paths, Permissions | `docs/user/agents.md` |
| How each harness is invoked (903), JSON output contract (961) | `docs/dev/building-and-testing.md` (internals; per R3) |
| YAML contract in brief (1071), Workflows built in/by name (1260), Built-in workflow template (1342) | `docs/user/workflows.md` (links `docs/dev/workflow-schema.md`) |
| Configuration (1097) | `docs/user/configuration.md` |
| On-disk layout (1228) | `docs/user/configuration.md` (user-visible) with internals to `docs/dev/architecture.md` pointer |
| git/libgit2 rationale | `docs/dev/building-and-testing.md` |
| "What the MVP does not do yet" (1364) | verify each claim against code (R6); keep still-true items as "Known limitations" in `docs/dev/building-and-testing.md` or `docs/user/workflows.md`; drop stale ones; nothing in README |

- Move text verbatim apart from fixing relative links/anchors; fold `conductor.md`/`remote.md` into `cli.md`/`tui.md` only if under ~40 lines.
- Verification of "ignored tests failing" claim: run `cargo test -- --ignored` (or inspect) before keeping it.
- Checklist: after moving, a script lists README H2/H3 titles of the old version (`git show HEAD:README.md`) and confirms each appears in some new doc.

### U4. New README.md (<=120 lines, English)
Name; 2-3 sentence description; feature list (A6: verify each against code/docs: YAML workflows, agent steps for Claude Code/Codex/OpenCode/GitHub Copilot harnesses, human steps/approvals, CLI + TUI, resume/recover, agent conversation, conductor, repos/workspaces, remote backends/server, harness usage limits, built-in workflows); Install (pinned one-liner, roles local/frontend/server, uninstall, requirements: x86_64 Linux glibc, `git`, harness on PATH); 3-4 command quick start; links to `docs/README.md` sections. No keybindings, flag tables, build/test, internals.

### U5. CONTRIBUTING.md and docs/README.md
- `CONTRIBUTING.md`: dev setup, build/test commands, ignored tests, pointer to `docs/dev/architecture.md`, release checklist (bump pinned tag in README + installation.md).
- `docs/README.md`: index of user / dev / operations docs; mention brainstorms/plans/solutions are Polish working artifacts.

### U6. Reference repair
- `grep -rnE 'ARCHITECTURE_DECISIONS|DOMAIN_MODEL|WORKFLOW_SCHEMA|README\.md#?'` outside historical dirs: ~25 Rust files (comments citing `WORKFLOW_SCHEMA.md`/`ARCHITECTURE_DECISIONS.md`, e.g. `engine/runner.rs`, `workflow/validator.rs`, `workspace/*`, `controller/profile_edit.rs`, `protocol/display.rs`, `runner/init.rs`), `docs/operations/remote-backends.md`, `docs/sources/claude-code-json.md`, and any non-historical docs such as `docs/follow-ups` (check each; leave dated Polish working artifacts untouched).
- Replace with new paths (comment-only edits in Rust; confirm no string reaching runtime/tests, including `README` anchors referenced from CLI help or error messages - grep `README` in `crates/**` and `tests/`; if a runtime string cites a README anchor, point it to the new doc, which is a text change only).
- Historical brainstorms/plans: leave as-is.

### U7. Link check
- Add a small script/test (`tests/docs_links.rs`, so it runs under `cargo test`) validating relative markdown links and heading anchors in root `*.md` and `docs/{user,dev,operations}/*.md`, `docs/README.md`. Run it; fix failures.

## Verification
- `wc -l README.md` <= 120; no `releases/latest/download` in README or docs outside the explanatory note.
- Install URL returns 200 (U1).
- Root contains only `README.md`, `LICENSE`, `CONTRIBUTING.md` as markdown.
- Link check passes; `cargo build` and `cargo test` (non-ignored) pass since Rust files change comments only (U6); guard test passes.
- All new/moved docs contain no Polish text (spot check for diacritics `[ąćęłńóśźż]` in `docs/user`, `docs/dev`, README, CONTRIBUTING).

## Risks
- Translating ~1450 lines faithfully: mitigate by section-count diff and keeping IDs.
- Pinned tag staleness: mitigated by the guard test and release checklist.
- Cargo version (0.1.0) does not track release tags: guard checks tag format/consistency only, per Decisions.
