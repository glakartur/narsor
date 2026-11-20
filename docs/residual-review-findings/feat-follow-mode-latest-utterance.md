# Residual review findings — feat/follow-mode-latest-utterance

Source: `ce-code-review mode:autofix` run `20261120-233921-4be2470d` against plan
`docs/plans/2026-11-20-003-feat-follow-mode-latest-utterance-plan.md`. Two `safe_auto`
fixes were applied (commit "fix(review): apply autofix feedback"); the findings below
were left for follow-up.

## Residual Review Findings

- **P2** `crates/narsor-web/ui/js/ui/tabs/conversation.js:175` — WebUI bottom anchor drifts when the conversation box grows near the end — https://github.com/glakartur/narsor/issues/123
- **P2** `src/tui/task_app/tabs/conversation.rs:682` — Rozmowa follow-mode auto-expansion opt-in has no test — https://github.com/glakartur/narsor/issues/124
- **P3** `crates/narsor-web/ui/js/ui/app.js:130` — WebUI Home/End swallowed when no conversation is visible — https://github.com/glakartur/narsor/issues/125
