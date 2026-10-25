---
title: "Claude Code --json-schema rejects a top-level anyOf: use a flat object schema and validate the answer in narsor"
date: 2026-10-17
category: integration-issues
module: engine/actions/agent
problem_type: integration_issue
component: tooling
related_components:
  - service_object
  - testing_framework
symptoms:
  - "`claude --json-schema '{\"anyOf\":[...]}'` exits with code 1 and a result line with `is_error: true`, `terminal_reason: api_error`"
  - "`API Error: 400 tools.9.custom.input_schema.type: Field required` when the schema root has no `type`"
  - "`API Error: 400 tools.9.custom.input_schema: input_schema does not support oneOf, allOf, or anyOf at the top level` when the root is `type: object` plus `anyOf`"
  - "The failed result line still has `subtype: success`, so a check on subtype alone reports success"
root_cause: wrong_api
resolution_type: code_fix
severity: medium
tags: [claude-code, json-schema, structured-output, anyof, input-schema, stream-json, output-contract, real-cli-recording]
---

# Claude Code --json-schema rejects a top-level anyOf: use a flat object schema and validate the answer in narsor

## Problem

Narsor passes the output contract of a step to `claude --print` as `--json-schema`. The contract has two shapes: a label answer (`output`, `data`, `variables`, `artifacts`) or a question (`status: awaiting_input`, `question`, `options`). The plan expressed this as a top-level `anyOf`. The real `claude` 2.1.302 rejects that schema, and every Claude step fails before the model runs.

## Symptoms

- With `{"anyOf":[{label branch},{awaiting_input branch}]}`: exit code 1, and the `result` line has `is_error: true`, `terminal_reason: "api_error"`, `api_error_status: 400` and the text `API Error: 400 tools.9.custom.input_schema.type: Field required`.
- With `{"type":"object","anyOf":[...]}`: the same failure with `API Error: 400 tools.9.custom.input_schema: input_schema does not support oneOf, allOf, or anyOf at the top level`.
- In both cases the `result` line has `subtype: "success"`. Only `is_error` and `terminal_reason` show the failure.

## What Didn't Work

- A top-level `anyOf` with one branch per answer shape (error: `input_schema.type: Field required`).
- The same `anyOf` with `"type":"object"` added at the root (error: top-level `anyOf`/`oneOf`/`allOf` not supported).
- Tests with the fake harness (`tests/fixtures/harness/fake_harness.sh`). The fake harness accepts any argument, so it could not show either failure. Only recordings of the real CLI showed it.

## Solution

Use one flat object with the optional properties of both branches, no `required`, and `additionalProperties: false`. Narsor decides which branch the answer is and whether it is complete, with the same rules it uses for a JSON answer found in text.

Step schema, from `output_schema()` in `crates/narsor-engine/src/engine/actions/agent.rs`:

```rust
json!({
    "type": "object",
    "properties": {
        "output": output, // {"type":"string","enum":[labels]}; no enum when the step declares no outputs
        "data": {"type": "object"},
        "variables": {"type": "object"},
        "artifacts": {"type": "array"},
        "status": {"type": "string", "enum": [AWAITING_INPUT]},
        "question": {"type": "string"},
        "options": {"type": "array", "items": {"type": "string"}},
    },
    "additionalProperties": false,
})
```

Conductor schema, from `answer_schema(purpose)` in `crates/narsor-engine/src/conductor/prompt.rs`: one flat object per purpose. It merges the fields of every decision that the purpose allows and has a top-level `"required": ["decision"]`. All 5 purposes were checked against the real CLI.

Reading the answer:

1. `ResultLine::take` (`harness/claude_code/classify.rs`) takes `structured_output` from the `result` line and ignores `null`. It goes into `HarnessRun.structured_output`.
2. `read_response(run)` (`agent.rs`) tries `structured_output` first, through the same `is_answer_object` / `response_from_object` rules as the text path.
3. When `structured_output` is absent or unusable, it logs a warning and falls back to `extract_response(&run.text)`. After that come the reprompt and the conductor, unchanged (R11 of the plan).

The text contract stays in the prompt for every harness. Opencode and codex ignore the schema.

## Why This Works

The CLI implements `--json-schema` as a custom tool named `StructuredOutput`, and the schema you pass becomes that tool's `input_schema`. The Anthropic API requires a tool `input_schema` to be `type: object` and forbids `anyOf`, `oneOf` and `allOf` at the top level. A flat object is a valid tool schema.

Keywords confirmed to work (CLI 2.1.302): root `type: object`, `properties`, `enum`, `additionalProperties: false`, `items`, nested `required` and `enum`, top-level `required`, and `{"type":"array"}` without `items`.

A flat schema cannot say "either A or B", so narsor must still check the answer. That check already existed for answers in text.

## Prevention

- Record real CLI runs before designing against a new flag. The fake harness proves nothing about the CLI (see also `runner-private-home-hides-harness-login-2026-10-10.md`). The recordings are in `tests/fixtures/claude-code/`, with the facts in its `README.md`.
- Detect a failed Claude run from `is_error` and `terminal_reason`, never from `subtype`.
- Keep the schema flat, keep the text contract in the prompt, and keep the text fallback. The schema may be refused or ignored by a future CLI version.
- `StructuredOutput` calls count as turns (`num_turns` 2–3 for a short answer). Size `max_turns` with this in mind.
- Tests: `tests/claude_code_classification.rs` runs on the recordings. The ignored test `the_real_claude_binary_answers_through_the_json_schema` in `tests/actions_agent.rs` drives the real binary. Run it by hand after a CLI upgrade.

## Other stream-json facts (CLI 2.1.302, recorded 2026-10-16)

- `structured_output` is on the `result` line, also with `--resume` and `--permission-mode plan`. The `result` text holds the same JSON.
- The model calls `tool_use` `StructuredOutput`, and the CLI answers `Structured output provided successfully`. When the model ends a turn without the tool, the CLI adds a `user` line with `isSynthetic: true` and `[structured-output-enforce] You MUST call the StructuredOutput tool…`.
- On a schema mismatch, the `tool_result` has `is_error: true` and `Output does not match required schema: /output: must be equal to one of the allowed values: [...]`, and the model tries again. What the CLI does when the retries run out is not known.
- `AskUserQuestion` is not available in `--print`. The model calls `ToolSearch` (`select:AskUserQuestion`), gets `No matching deferred tools found` and asks in plain text. The output contract's `awaiting_input` is the real route for questions. Since 2026-10-25 the `claude-code` adapter adds a note to the contract (`HarnessAdapter::contract_note`): the answer and every question go through `StructuredOutput`.
- `thinking` blocks have empty text and only a signature.
- An `error_max_turns` result has no `result` field. Its `errors[]` holds plain strings, not objects with `message`.
- `system` subtypes seen: `init`, `hook_started`, `hook_response`, `thinking_tokens`, `permission_denied`.

## Related Issues

- `ARCHITECTURE_DECISIONS.md` D60 (unified conversation model, `--json-schema`) and D55 (conductor answers as structured JSON).
- Plan: `docs/plans/2026-10-16-2343-feat-zunifikowany-model-rozmowy-plan.md` (U1, U5). Its `anyOf` design was replaced by the flat fallback that the plan already listed as a risk mitigation.
- `docs/solutions/integration-issues/runner-private-home-hides-harness-login-2026-10-10.md`: the same lesson that a fake harness does not prove real-CLI behaviour.
- `docs/sources/claude-code-json.md` is not a source of truth for the format.
