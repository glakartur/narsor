---
date: 2026-11-22
topic: task-state-preview
---

# Preview of the complete task state

## Summary

The owner of a task can see the complete state of the task's execution: every
part of the context (`variables`, `artifacts_index`, `workspace_info`), the
local variables of the steps and the conductor's state. The TUI, the web
interface and the CLI show it. The server is the only source: on request it
reads the state from the task's runner through the existing relay (the live
runner, or the read-only runner of D63 for a stopped task). All interfaces show
the state as one flat, sorted list of `path: value` rows, for example:

```
context:variables:var_name: value
context:variables:var2: val2
```

In the TUI a multi-line value is folded to one line with the same marker as the
conversation view. A cursor moves over the rows, and `Enter` unfolds the
selected value to its full multi-line form.

---

## Problem Frame

Today the owner sees only parts of the state:

- `narsor-cli task show --details` prints the context variables
  (`src/cli/commands/task_read.rs`), from `GET /control/v1/tasks/{id}/execution-details`.
  Values are printed as JSON on one line, so a long multi-line value (a plan, a
  summary from an agent) is hard to read.
- The TUI and the web interface do not show the context variables at all.
- Nothing shows `artifacts_index`, `workspace_info`, the step locals
  (`metadata.engine.step_locals` in `state.json`) or the conductor's internal
  state (`ConductorState`: the invocation under way, the queue, the counters of
  attempts, the used retries, the decisions not yet applied, the failure
  report).

When a workflow goes wrong (a wrong branch of a `when`, a template that renders
empty text, a conductor that does not answer), the operator must open the task
storage on disk or in the pod. That needs access to the machine or the cluster,
and it does not work for the owner of a remote task.

---

## Actors

- A1. Task owner: opens the state preview in the TUI, the web interface or the
  CLI and reads it.
- A2. Control plane (`narsor-server`): receives the request, checks ownership,
  asks the runner through the relay and returns the answer.
- A3. Runner (`narsor-runner`, live or read-only): reads `context.json`,
  `state.json` and the conductor store of its task storage and uploads the
  state.

---

## Key Flows

- F1. Preview of a running task
  - **Trigger:** The owner opens the *Stan* tab in the TUI (or the state view
    in the web interface, or runs the CLI command).
  - **Actors:** A1, A2, A3
  - **Steps:** The client asks the server. The server opens a relay request
    and wakes the runner's long-poll. The runner reads its storage and uploads
    the state. The server cleans the texts and answers. The client flattens
    and shows the rows.
  - **Outcome:** The owner sees the state as it was at the moment of the read.
  - **Covered by:** R1, R2, R4, R6

- F2. Preview of a stopped task
  - **Trigger:** Same as F1, for a paused, failed or finished task that is not
    archived.
  - **Actors:** A1, A2, A3
  - **Steps:** The server has the controller start a read-only runner (D63).
    Until it connects, the answer is `reader_starting` and the client asks
    again. Then as in F1.
  - **Outcome:** Same as F1.
  - **Covered by:** R2, R3

- F3. Reading a long value in the TUI
  - **Trigger:** A row shows a folded multi-line value.
  - **Actors:** A1
  - **Steps:** The owner moves the cursor to the row and presses `Enter`. The
    row unfolds to all lines of the value. `Enter` again folds it.
  - **Outcome:** The owner reads the full value without leaving the tab.
  - **Covered by:** R11, R12

---

## Requirements

**Source of data**

- R1. The server gives a new owner-only read of the complete execution state
  of one task. Only the owner of the task (the same check as
  `execution-details`) can read it.
- R2. The server does not keep or cache the state. At each request it asks
  the task's runner through the relay, in the same way as `execution-details`
  and `conductor`: the live runner while the task runs, the read-only runner
  (D63) while it is stopped.
- R3. When the state cannot be read, the answer says why, with the reasons
  that the existing reads use (`not_started`, `reader_starting`, `archived`,
  the other reasons of `ConversationUnavailableReason`). A runner that does not
  answer in time gives a retryable `503`, as today.
- R4. The state contains:
  - `context`: all sections of `context.json` — `variables`,
    `artifacts_index`, `workspace_info`, and any section added later.
  - `locals`: the step locals, per step (`state_id → { NAME: value }`).
  - `conductor`: all fields of the conductor's state (`ConductorState`) and
    the conductor's generated title, when it has one. When the task has no
    conductor, the section is empty and the answer says so.
- R5. The server cleans the texts of the state before it answers (names and
  values), in the same way as `execution-details` does today
  (`without_unsafe_chars`, `without_unsafe_chars_json`).

**Flat list (all interfaces)**

- R6. Each interface shows the state as a flat list of rows `path: value`.
  The path joins the keys with `:`, starting with the section:
  `context:variables:NAME`, `context:artifacts_index:NAME:path`,
  `locals:STATE_ID:NAME`, `conductor:failure:reason`.
- R7. JSON objects are flattened down to their leaves. A JSON array stays one
  row: its value is the array as compact JSON. An empty object is one row with
  the value `{}`.
- R8. A string value is shown as its text, without JSON quotes and escapes, so
  that a multi-line string has its real lines. Other values (number, bool,
  `null`, array) are shown as compact JSON. Rows are sorted by path, with the
  sections in the fixed order `context`, `locals`, `conductor`.
- R9. One flattening function produces the rows for the TUI and the CLI, so
  both show the same paths and values. The web interface follows the same
  rules (R6–R8).

**TUI**

- R10. The task view has a new tab *Stan*. It reads the state when the owner
  opens it and when the owner asks for a refresh (the refresh key of the other
  tabs). It does not read the state again in the background. The one exception
  is the answer `reader_starting`: then the tab asks again until it gets the
  state or another reason (F2, AE3).
- R11. A row whose value has more than one line, or one line wider than the
  screen, shows only the first line, clipped to the width, with the collapsed
  marker of the conversation view (`›`). An unfolded row uses the expanded
  marker (`⌄`) and shows every line of the value, wrapped to the width. A
  value that fits on one line has the neutral marker (`·`).
- R12. A cursor selects one row. The keys that move in the conversation view
  move it (line up and down, page up and down, start and end). `Enter` folds or
  unfolds the selected row. When the selected row is unfolded and taller than
  the screen, the movement keys first scroll inside that row, and move the
  cursor to the next row only when the first or last line of the value is in
  view. The rows unfolded stay unfolded after a refresh when their path still
  exists.
- R13. The tab shows the reason of R3 when there is no state, and the time
  of the read when there is one.

**Web interface**

- R14. The task view of the web interface has the same list. A row that R11
  folds in the TUI is folded to one line with the same marker. A click (or
  `Enter` on the focused row) unfolds it, and the same action folds it again.
  Like the *Stan* tab (R13), the view shows the reason of R3 when there is no
  state, and the time of the read when there is one.

**CLI**

- R15. A CLI command prints the flat list of one task, one row per line. A
  multi-line value is printed in full: its next lines are indented under the
  row. A `--json` option prints the unflattened state as JSON. When there is no
  state, the command prints the reason of R3 to the error output and exits with
  a non-zero status.

---

## Acceptance Examples

- AE1. **Covers R6, R7, R8.** Given a context with
  `variables = { "branch": "feat/x", "pr": 12, "files": ["a", "b"] }`, the
  list has the rows `context:variables:branch: feat/x`,
  `context:variables:files: ["a","b"]` and `context:variables:pr: 12`, in this
  order.
- AE2. **Covers R11, R12.** Given a variable `plan` with three lines, the
  *Stan* tab shows `› context:variables:plan: <first line>`. With the cursor on
  the row, `Enter` shows `⌄ context:variables:plan:` with all three lines;
  `Enter` again folds it.
- AE3. **Covers R2, R3.** Given a paused task whose read-only runner is not
  yet connected, the *Stan* tab says that the reader is starting and asks
  again; when the runner connects, it shows the state.
- AE4. **Covers R3.** Given an archived task, every interface says that the
  storage was deleted and shows no rows.
- AE5. **Covers R4.** Given a task whose conductor waits on a failed step, the
  list has rows under `conductor:failure:` and `conductor:pending:`.

---

## Success Criteria

- The owner can answer "what value does the workflow see now?" for any
  variable of a local or Kubernetes task without access to the task storage.
- The TUI, the web interface and the CLI show the same rows for the same
  state.
- A multi-line value of hundreds of lines does not make the *Stan* tab hard to
  scan: it takes one row until the owner unfolds it.

---

## Scope Boundaries

- Read-only. The preview does not edit a variable or the conductor's state.
- No live updates: the state is read on demand (R2, R10).
- No search or filter in the list, no copy to the clipboard.
- The conductor's chat, proposals and decisions stay in the *Dyrygent* tab;
  this feature does not change that tab. The *Stan* tab shows only the raw
  fields of `ConductorState` (R4), including the decisions not yet applied.
- No change to `task show --details` and to the `execution-details` read.
- No history of the state (values at an earlier step).
- Artifact contents are not shown; only their entries in `artifacts_index`.

---

## Key Decisions

- **A new read, not a wider `execution-details`.** `execution-details` feeds
  the summary of a task (pending input, problem, activity). The complete state
  can be much larger and is read only when the owner asks. A separate relay
  read keeps the summary fast. The pattern is the same (`reads/details.rs`,
  `reads/conductor.rs`).
- **The runner sends structured state; the client flattens it.** The wire
  carries JSON sections, so `--json` and future clients get the structure. One
  shared flattening function (R9) keeps the TUI and the CLI the same.
- **Strings unquoted, other values as JSON.** The owner reads text as text,
  and a multi-line string keeps its real lines. The cost is in Z5.

---

## Dependencies / Assumptions

- The relay and the read-only runner of D63 work for this read in the same way
  as for `execution-details` and `conductor`.
- The context does not store large blobs (domain model §2.6), so one read is
  small in a normal task. A limit protects the server anyway (see the
  assumptions).

---

## Założenia do potwierdzenia

- Z1. "Wszystkie interfejsy frontowe" means the TUI, the web interface and the
  CLI. The CLI gets a text form of the same list (R15).
- Z2. "Complete state" also includes the step locals (`locals:`), not only the
  context and the conductor. They live in `state.json` beside the context, and
  they are part of what a step sees. If they are not wanted, R4 drops one
  section.
- Z3. "Zmienne dyrygenta" means the fields of the conductor's state file
  (`ConductorState`) and its generated title. The chat is not part of it: the
  *Dyrygent* tab already shows it.
- Z4. A JSON array stays one row (R7). Context variables hold simple types and
  short lists; a row per index would be noise. Objects are flattened to their
  leaves because the example in the request uses nested paths.
- Z5. A string is shown without quotes (R8). As a result `12` (number) and
  `"12"` (string) both show as `12`. This is acceptable for a preview; `--json`
  in the CLI shows the exact types.
- Z6. The new TUI tab is named *Stan* and placed after *Dyrygent*, before
  *Info*. TUI labels are in Polish, as the other tabs.
- Z7. The state is read when the tab opens and on a manual refresh, not in the
  background (R10). This keeps the load on the runner the same as today.
- Z8. The server rejects a state larger than a fixed limit with a clear error,
  and does not cut it silently. The exact limit (in the range of a few MiB) is
  set in planning.
- Z9. Only the task owner can read the state, as with the other details reads.
  The context can contain sensitive text written by agents; the preview does
  not hide values, because the owner can already see them with
  `task show --details`.
- Z10. The web interface follows the same fold and unfold rules as the TUI,
  with a click as the unfold action (R14).

---

## Pytania otwarte dla użytkownika

None. Each unclear point has an assumption in the section above.

---

## Outstanding Questions

### Deferred to Planning

- The name of the endpoint and the CLI command (for example
  `GET /control/v1/tasks/{id}/state` and `narsor-cli task state TASK_ID`).
- The wire type of the state and where it lives in `narsor-protocol`
  (`controller/relay.rs` beside `ExecutionDetails`).
- Where the shared flattening function lives so that the TUI and the CLI use
  it (the client library in `src/`, or `narsor-protocol`).
- The size limit of Z8 and the timeout of the relay read (the existing
  `conversation_page_timeout` or a new one).
- The interval and the retry limit of the `reader_starting` retry (R10).
- How the web interface reuses its existing collapsing UI, if it has one.

---

## Deferred / Open Questions

These entries are review items left for the author. They do not block implementation.

### From 2026-11-22 review

- **The reason for not masking values is wrong for most sections** — Z9 (P2, coherence, adversarial, confidence 75)

  The preview shows the owner data that no existing read shows, while Z9 says nothing new is exposed. Today `task show --details` returns only the context variables. The preview adds `workspace_info`, the step locals and the conductor's queued and pending request payloads. Decision needed: confirm that the owner may see these sections, or drop them (Z2 already allows dropping a section). Then limit the "already visible" reasoning in Z9 to `context:variables`.

- **The size limit of Z8 has three competing remedies** — Z8 (P2, scope-guardian, feasibility, adversarial, confidence 75)

  Without a decision, planning sets a limit whose failure mode is unclear. Scope-guardian proposes to drop the dedicated limit, because the existing body limit applies and an oversized read fails visibly for the one owner who can act on it. Feasibility notes that the runner upload route has a default body limit of about 2 MiB, so a state between 2 MiB and the chosen limit ends as a retryable `503` and not as the clear error that Z8 promises. Adversarial proposes a limit per value, so that one oversized variable does not hide the rest of the state. Decision needed: keep one limit for the whole state (and report oversize through the relay-failure path), drop the limit, or limit per value.

- **A conductor array of objects becomes one unreadable row** — R7 (P2, adversarial, confidence 75)

  The `queue` and `unapplied_decisions` fields of `ConductorState` are arrays of objects, and each queued request carries a free-form payload. R7 turns each array into one compact-JSON row, so nested multi-line text shows as escaped `\n` and cannot be unfolded. Change: an array whose items are all scalars stays one row, and an array with any object is flattened to one row per index (`conductor:queue:0:payload:...`). Update Z4 to give this reason. This changes a rule that R7 and Z4 state explicitly, so it needs approval.

- **A section that fails to load looks empty** — R3, R4, F1 (P2, adversarial, confidence 75)

  R3 covers only a failed whole read. If one of the three sources (`context.json`, `state.json`, the conductor store) cannot load, the section can show as missing, and the owner can conclude that there is no conductor failure. The sources are written separately, so "as it was at the moment of the read" in F1 can also be false during a step transition. Change: each section reports its own read status, and a section that fails to load is returned as unreadable with a reason, never as empty. Replace the F1 wording with "each section read separately".

### FYI (no decision required)

- The sort rule in R8 can be read as one global sort by path, which would put `conductor` first. A sort within each section is the intended reading.
- The flattening rules (R6–R8) are implemented twice, in Rust and in the web UI. A shared set of input and expected-row fixtures, starting with AE1, would keep the interfaces equal.
