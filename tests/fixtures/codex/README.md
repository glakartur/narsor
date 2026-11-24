# codex recordings

Recorded 2026-11-24 with `codex-cli 0.171.0` for the unified harness `run`
(plan `docs/plans/2026-11-24-001-refactor-unified-harness-run-and-conductor-context-files-plan.md`, U2).
Paths are rewritten to `/tmp/rec`.

- `exec_prompt_on_stdin_unauthenticated.jsonl` —
  `printf '<prompt>' | codex exec --sandbox read-only --config approval_policy=never --json --skip-git-repo-check -`.
  The machine had no codex login, so the turn fails with 401, but
  `thread.started` and `turn.started` show that `-` takes the prompt from
  stdin and starts the turn.

Read-only sandbox reading outside the working directory, checked with the
sandbox codex uses for `--sandbox read-only` (default policy):

```text
$ cd /tmp/rec/cwd && codex sandbox -- cat /tmp/rec/ctx/notes.md
The secret word is PELICAN-42.
$ codex sandbox -- sh -c "echo x > /tmp/rec/ctx/w.txt"
sh: /tmp/rec/ctx/w.txt: Read-only file system
```

So the conductor's context files outside the workspace are readable under
`--sandbox read-only` without `--add-dir` (which would grant write access).
