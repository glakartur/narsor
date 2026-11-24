# opencode recordings

Recorded 2026-11-24 with `opencode v2.4.1`, model
`opencode/deepseek-v4.1-flash-free`, for the unified harness `run`
(plan `docs/plans/2026-11-24-001-refactor-unified-harness-run-and-conductor-context-files-plan.md`, U2).
Paths are rewritten to `/tmp/rec`.

- `run_prompt_on_stdin.jsonl` —
  `printf 'Answer with exactly the text DONE-FROM-STDIN.' | opencode run --model <m> --format json`
  (no message argument). The prompt on stdin is the message: the answer is
  `DONE-FROM-STDIN`.
- `run_prompt_in_attached_file.jsonl` —
  `opencode run --model <m> --format json --file /tmp/rec/ctx/prompt.md "Follow the instructions in the attached file."`.
  The attached file reaches the model, but as an attachment next to the
  short message, not as the message itself.

Decision: opencode takes a long prompt on stdin (same as claude and codex),
not through `--file`.
