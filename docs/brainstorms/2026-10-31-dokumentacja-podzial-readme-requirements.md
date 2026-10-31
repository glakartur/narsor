---
date: 2026-10-31
topic: dokumentacja-podzial-readme
---

# Documentation split: user-facing README + separate docs

## Summary

README.md (1375 lines) mixes user intro with CLI/TUI reference, architecture, config. Rewrite README as short user-facing page: name, what Narsor is, features, correct install. Move rest to `docs/` by audience. All docs in English.

---

## Problem

- README too technical: build/test, cargo, git/libgit2 rationale, per-command CLI reference, TUI keybindings, on-disk layout, YAML contract, exit codes.
- Install `curl` link wrong: `releases/latest/download/install.sh` 404s — all releases (v0.2.3…v0.4.0) are marked Pre-release; GitHub `latest` skips them. Same URL repeated in `docs/operations/installation.md`, `dist/install.sh` (default `version="latest"`), `.github/workflows/release.yml`, `tests/install_script.rs`.
- Existing design docs (`ARCHITECTURE_DECISIONS.md`, `DOMAIN_MODEL.md`, `WORKFLOW_SCHEMA.md`) in repo root, written in Polish. Brainstorms/plans in `docs/` also Polish (history, leave).

---

## Requirements

**R1 README.md content (user-facing, English)**
- Name + 2–3 sentence description: workflow engine for tasks done by AI agents and people; YAML process graph; persisted state/events; recoverable, auditable.
- Features list (user view): YAML workflows; agent steps (Claude Code, Codex, OpenCode, GitHub Copilot harnesses); human steps/approvals; CLI + full-screen TUI; resume/recover; conversation with agent; conductor; repos/workspaces; remote backends/server; harness usage limits; built-in workflows.
- Install: working one-liner, roles (local/frontend/server), uninstall, requirements (x86_64 Linux glibc, `git`, harness on PATH for agent steps).
- Minimal quick start (3–4 commands) max.
- Links to docs. No keybindings, no flag tables, no build/test, no internals.
- Target ≲120 lines.

**R2 Correct install link**
- Find working URL. Verify with `curl -fsSL -o /dev/null -w '%{http_code}'` before writing.
- Candidate: tag-pinned `releases/download/<tag>/install.sh` (asset exists, tag baked in by release.yml, so binaries resolve from that tag). Raw `main/dist/install.sh` is not a valid alternative as-is: its default `version="latest"` makes it fetch binaries from `releases/latest/download`, which 404s while all releases are pre-releases (it would need `NARSOR_VERSION=<tag>` / `--version`).
- A tag-pinned URL goes stale each release; README must state how the tag is kept current (or use Q1's option) — decide before writing.
- Root cause fix (`latest` skipping pre-releases) is out of doc scope unless trivial; see Q1.
- Align same URL in `docs/operations/installation.md`.

**R3 Docs layout (standard files at root, rest in `docs/`)**
- Root standard files: `README.md`, `LICENSE` (exists). Add `CONTRIBUTING.md` (dev setup, build/test, pointer to architecture) — standard file for contributors.
- User docs `docs/user/`:
  - `cli.md` — `narsor-cli` commands, global options, exit codes (from README).
  - `tui.md` — TUI usage, keys (from README).
  - `configuration.md` — config file, settings, on-disk layout (user-visible parts).
  - `workflows.md` — writing/using workflows, built-ins, YAML contract brief; links to schema reference.
  - `agents.md` — AgentAction usage, harnesses, sessions, permissions.
  - `conductor.md`, `remote.md` (remote tasks/backends) — or fold into above if small.
- Developer docs `docs/dev/`:
  - `architecture.md` — move/translate `ARCHITECTURE_DECISIONS.md`.
  - `domain-model.md` — move/translate `DOMAIN_MODEL.md`.
  - `workflow-schema.md` — move/translate `WORKFLOW_SCHEMA.md`.
  - `building-and-testing.md` — build, tests, ignored tests, binaries layout, git/libgit2 rationale, harness invocation internals/JSON contract.
- Ops docs stay `docs/operations/` (installation, remote-backends) — already fits.
- `docs/brainstorms|plans|solutions|…` unchanged (workflow artifacts).
- Add `docs/README.md` index (standard index file for docs folder).

**R4 Language**
- All new/moved docs English, incl. translated root design docs. Preserve content faithfully; normative status kept.
- Historical Polish brainstorms/plans/solutions not translated (Q2).

**R5 Link integrity**
- Update all references to moved files (`ARCHITECTURE_DECISIONS.md`, `DOMAIN_MODEL.md`, `WORKFLOW_SCHEMA.md`, README anchors) across repo: code comments, docs, tests (`grep -rn`). Leave `git mv` for history.
- Check in-code doc refs (e.g. Rust comments citing `D12`, `WORKFLOW_SCHEMA.md`).
- No content loss: every README section lands somewhere.

**R6 Accuracy**
- README "MVP does not do yet" and stale statements (e.g. ignored tests failing) → verify or move to dev docs; no promises in README.

---

## Scope boundaries

- No code behavior change.
- No translation of historical `docs/{brainstorms,plans,…}`.
- No new features invented for README; only existing ones.

---

## Success criteria

- README ≲120 lines, no tech internals, correct working install command.
- Every moved section reachable from `docs/README.md`; no broken relative links (link check script).
- Root has no design/reference docs (only `README.md`, `LICENSE`, `CONTRIBUTING.md` plus existing source/tooling dirs and files); the three design docs live under `docs/dev/`.
- All new docs English.

---

## Założenia do potwierdzenia

- A1. Install URL: use the verified tag-pinned release asset URL — releases are pre-release so `/latest/` fails (raw `main` fails too unless version is pinned).
- A2. Root design docs (3 files, Polish) moved to `docs/dev/` and translated to English.
- A3. Historical Polish brainstorms/plans untouched.
- A4. `CONTRIBUTING.md` added at root as the standard dev-entry file.
- A5. Docs folder naming: `docs/user/`, `docs/dev/`, existing `docs/operations/` kept.
- A6. Feature list in README limited to implemented features verified in code/docs.
- A7. No change to `install.sh` default `latest` behavior in this task.

## Pytania otwarte dla użytkownika

- Q1. Mark a release as non-prerelease (so `releases/latest/download/install.sh` works) or keep pinned/raw URL in README?
- Q2. Translate historical Polish docs in `docs/brainstorms|plans|solutions` too, or leave?
- Q3. Keep Polish originals of design docs anywhere, or English only?
