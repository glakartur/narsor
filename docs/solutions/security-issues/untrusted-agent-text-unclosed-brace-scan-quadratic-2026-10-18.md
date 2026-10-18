---
title: "Agent output text is untrusted: bound the brace scans and strip control characters before printing"
date: 2026-10-18
category: security-issues
module: engine/actions/agent
problem_type: security_issue
component: service_object
related_components:
  - tooling
  - testing_framework
severity: high
symptoms:
  - "A security review flagged the JSON-object scanners in agent.rs as O(n^2) on agent output full of unclosed `{`"
  - "A text of 200000 `{` followed by a contract makes the label and reply scans read to the end of the text once per brace"
  - "`narsor-cli` printed pending human input (instruction with a step reply, problem reason, answer tails) with raw control characters"
root_cause: missing_validation
resolution_type: code_fix
tags: [untrusted-input, agent-output, algorithmic-dos, quadratic-scan, brace-scan, output-contract, terminal-escape, sanitize]
---

# Agent output text is untrusted: bound the brace scans and strip control characters before printing

## Problem

Text from an agent is untrusted: an LLM, or a prompt-injected repository, controls it. The engine scans that text for JSON objects in `crates/narsor-engine/src/engine/actions/agent.rs`:

- `json_objects` finds the output label (`extract_response`; `conductor/response.rs` uses it too).
- `final_contract_start` and `last_contract_span` find the output-contract object, and `clean_reply` cuts it out of the step's reply. Later prompts and human gates receive that reply through `{{ steps.<step>.reply }}`.

A review fix let these scanners retry after a `{` that never closes. Each retry scanned to the end of the text, so a text full of unclosed braces took quadratic time: for 200 000 braces, tens of billions of character steps. There is no error and no crash; the engine thread only gets slow. An automated security review flagged it as algorithmic DoS.

The same review found that `narsor-cli` printed pending human input without removing terminal control characters. That input now can carry a step's reply, and its problem carries agent answers, so agent text could send escape sequences to the operator's terminal.

## What Didn't Work

1. **The original single-pass scan.** It tracked brace depth from the first `{`. One stray `{` in the prose kept the depth above 0, so a valid contract after it stayed hidden and the step got a missing-label reprompt.
2. **Restart at `start + 1` with no limit** (the "Before" code below). `final_contract_start` had the same shape.
3. **Only capping the text to its last 64 KiB.** The worst case is still about 2·10⁹ steps.
4. **One linear pass with a global brace stack.** It breaks recovery: a stray `"` in the prose flips the quote state for everything after it. Each candidate must start with a fresh, unquoted state, so the restarts stay and only their number can be bounded.

## Solution

Bound the restarts with one named constant, and share the scan between the label and the reply paths.

Before (same pattern in `json_objects`):

```rust
match object_end(text, start) {
    Some(end) => { /* keep the object */ from = end; }
    None => from = start + 1, // every miss scans to the end
}
```

After (doc comments shortened):

```rust
/// Braces that never close, skipped before the scan gives up.
const MAX_BRACE_CANDIDATES: usize = 64;

fn top_level_objects(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let (mut from, mut unclosed) = (0, 0);
    while let Some(offset) = text[from..].find('{') {
        let start = from + offset;
        match object_end(text, start) {
            Some(end) => {
                spans.push((start, end));
                from = end;
            }
            None if unclosed < MAX_BRACE_CANDIDATES => {
                unclosed += 1;
                from = start + 1;
            }
            None => break,
        }
    }
    spans
}
```

- `json_objects` parses each span from `top_level_objects`.
- `last_contract_span` takes the last span that is a contract.
- `final_contract_start` counts every `{` it tries from the right, balanced or not, and returns `None` on the 65th. Then `clean_reply` falls back to `last_contract_span`.

In the CLI, `pending_input_lines` (`src/cli/commands.rs`) cleans the whole serialized view once, before it formats any line:

```rust
let Ok(mut pending) = serde_json::to_value(pending) else { /* … */ };
// The instruction can carry a step's reply and the problem an agent's
// answer: text from an agent, never handed to a terminal as it is.
narsor_protocol::display::without_unsafe_chars_json(&mut pending);
```

`without_unsafe_chars_json` (`crates/narsor-protocol/src/display.rs`) works on every string and key:

- It keeps line feeds and turns tabs into spaces.
- It drops other control characters (also `\r` and ESC).
- It drops invisible and bidi format characters.

Because the cleaning happens before the answer tail is split into lines, a lone `\r` cannot overwrite a printed line.

Test: `a_text_full_of_unclosed_braces_is_read_in_linear_time` runs `json_objects` and `clean_reply` on 200 000 `{` plus a contract in under 2 seconds.

## Why This Works

- **Recovery still works.** Prose has few stray braces and the contract is near the end, so 64 retries keep recovery for realistic text.
- **The worst case is linear.** Each function makes at most about 65 scans, and each scan reads at most the whole text. The cost grows linearly with the text length, with a large constant factor.
- **No hang.** When the bound is hit, the worst result is a missing label (a normal reprompt) or a reply that keeps some contract text.

**Accepted trade-offs:**
- 64 is a chosen limit, not a derived one.
- A contract after more than 64 unclosed braces is not found.
- A contract with more than 64 `{` inside it (nested data or braces in strings) exhausts the right-to-left search. The fallback then removes the object but not a code fence around it.

## Prevention

- **A recovery or retry loop over untrusted text gets a work bound in the same change.** Use a named `const` with a doc comment that says what it protects, a counter and a `break`.
- **Add a timing test with a pathological input** for each entry point that reads the text, with a limit in seconds. The current test covers many unclosed `{`. Deep nesting and many unclosed `"` are not tested yet.
- **When a feature routes agent text to a new sink, clean it at that sink.** Step replies made agent text reach the human-input instruction, and so the CLI. Use the `narsor_protocol::display` sanitizers:
  - `without_unsafe_chars` for single-line text;
  - `without_unsafe_chars_multiline` for multi-line text;
  - `without_unsafe_chars_json` for a whole JSON view (this covers fields added later too).

**Known gap:** the review found other `narsor-cli` lines in `src/cli/commands.rs` that still print agent, conductor or runner text raw:
- the failure problem reason and its episode;
- the conductor summary and recommendation;
- chat entry text;
- `outcome.message`;
- context variable names.

`task_history.rs` already cleans the same chat text with `without_unsafe_chars_multiline`. Wrap these lines the same way.

## Related Issues

- [Claude Code's --json-schema rejects a top-level anyOf](../integration-issues/claude-code-json-schema-rejects-top-level-anyof-2026-10-17.md): why the engine still reads the answer from text, and the text-fallback path these scanners serve.
- [Bracketed paste into TUI text fields](../design-patterns/tui-bracketed-paste-into-text-fields-2026-10-18.md): the input side of the same rule. Text from outside is stored raw and cleaned where it is drawn.
- PR glakartur/narsor#4 (commit `a3edcb9`).
