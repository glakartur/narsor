---
artifact_contract: ce-unified-plan/v1
artifact_readiness: implementation-ready
execution: code
created_at: 2026-10-04T19:30:00+02:00
type: refactor
topic: tui-unified-transcript-event
---

# refactor: Unified transcript event for TUI conversations

## Goal Capsule

**Objective:** The operator reads a consistent conversation transcript regardless of whether a harness is Claude, Codex, OpenCode, or Narsor workflow infrastructure.

**Means:** Translate every retained harness record into one `TranscriptEvent` model with `kind`, `summary`, `details`, and lossless `raw`; render only that model.

**Authority:** User direction in this session: adapters translate harness-specific envelopes; `raw` is a field inside the single unified event, never the default rendered content.

**Stop conditions:** The renderer contains no Claude/Codex/OpenCode envelope parsing, incomplete adapter input remains readable in rendered mode, and raw mode exposes the original retained payload.

## Requirements

- R1. Each retained entry becomes one unified transcript event with `kind`, `summary`, `details`, and `raw`.
- R2. `raw` preserves the original harness payload losslessly and is available in raw mode.
- R3. Claude, Codex, and OpenCode adapters translate their envelopes into the common event model; renderer code does not inspect adapter-specific fields.
- R4. Missing optional adapter fields produce a generic semantic event, not visible source JSON.
- R5. Narsor/workflow messages use a separate normalized system-event vocabulary: agent input, step start/completion/failure, retry, and waiting-for-input.
- R6. Rendered mode uses `kind`, `summary`, and concise `details`; expanded mode shows full normalized details; raw mode shows `raw` only.

## Scope Boundaries

- Do not change persistence format, APIs, refresh behavior, transcript anchoring, or raw-byte retention.
- Do not add a renderer parser for future harness envelopes.
- Do not discard fields merely because they are not shown in the default view.

## Technical Design

```text
original harness payload
        |
        v
adapter (Claude / Codex / OpenCode / Narsor)
        |
        v
TranscriptEvent { kind, summary, details, raw }
        |
        +--> rendered / expanded transcript
        +--> raw transcript mode
```

`details` uses stable domain keys rather than harness keys, for example `tool.name`, `tool.command`, `tool.exit_code`, `workflow.step`, `workflow.attempt`, and `error.message`.

## Implementation Units

### U1. Define the unified event contract

**Files:** `src/tui/conversation.rs`, `tests/tui_conversation.rs`.

Replace the presentation-oriented classified text shape with a `TranscriptEvent` contract. Make the contract the sole adapter output and include original payload under `raw`. Add constructors/helpers that make absent optional fields explicit without forcing adapters to manufacture meanings.

**Test scenarios:** serialization contains all four fields; raw JSON and non-JSON payloads remain recoverable; generic events retain a nonempty summary.

### U2. Translate harness envelopes in adapters

**Files:** `src/tui/conversation.rs`, `tests/tui_conversation.rs`.

Split Claude, Codex, and OpenCode recognition into adapter functions that return the shared event. Extract common kinds and stable detail keys. For incomplete known events, emit a generic semantic summary in the known category rather than `Unknown` or a source envelope.

**Test scenarios:** full and sparse message, thinking, tool call, tool result, lifecycle, and error payloads from each harness; every case has readable summary and exact raw payload.

### U3. Normalize workflow/system messages

**Files:** `src/tui/conversation.rs`, relevant workflow/event source modules, `tests/tui_conversation.rs`, `tests/tui_state.rs`.

Introduce a Narsor system adapter for agent input and workflow lifecycle messages. Populate normalized `workflow` details instead of treating operational text as an agent utterance.

**Test scenarios:** input sent to agent, start, success, failure, retry, and waiting states render distinctly and preserve their source payload.

### U4. Render only the unified contract

**Files:** `src/tui/app.rs`, `src/tui/transcript.rs`, `src/tui/remote.rs`, `src/tui/views.rs`, `src/tui/remote_views.rs`, `tests/tui_state.rs`.

Move all displayed transcript content to `kind`, `summary`, and `details`. Keep raw mode and expansion behavior, but make expansion display normalized details rather than a parsed harness envelope. Verify local and remote views continue using the shared adapter path.

**Test scenarios:** identical normalized events render identically regardless of source adapter; expansion preserves anchors; raw mode stays terminal-safe and lossless.

## Verification Contract

- `cargo fmt --check`
- `cargo test --test tui_conversation`
- `cargo test --test tui_state`
- `cargo clippy --all-targets -- -D warnings`

## Definition of Done

- All adapters return the same event contract.
- No rendered-mode path displays a harness JSON envelope by default.
- System/workflow records have a readable, separate presentation.
- Raw mode remains lossless and all verification gates pass.
