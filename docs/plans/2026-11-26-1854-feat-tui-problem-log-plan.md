---
title: TUI Problem Log and Diagnostic Dump - Plan
type: feat
date: 2026-11-26
topic: tui-problem-log
artifact_contract: ce-unified-plan/v1
product_contract_source: ce-brainstorm
execution: code
---

# TUI Problem Log and Diagnostic Dump - Plan

## Goal Capsule

- **Objective:** When the TUI shows a server problem (for example `nie udało się wczytać przebiegu: brak połączenia z serwerem` on *Przebieg*), the operator can save, with one key, a file that tells what actually failed in the last 30 minutes, so the cause can be found instead of guessed.
- **Means:** An in-memory problem log inside the TUI process, with entries older than 30 minutes dropped automatically, and a global key `D` that writes the current log to a temporary file.
- **Product authority:** This plan covers the `narsor` TUI only. The server, the protocol, the Web UI and `narsor-cli` are not active scope. A fix of the root cause of the failed reads is not active scope (see Q1).
- **Open blockers:** Q1 (does this work also change the visible message, or only collect evidence). The plan is written for the safest answer (evidence only); planning can start on it.

---

## Product Contract

### Summary

The TUI records every problem it meets while it runs (failed server calls and errors it shows to the operator) in memory, with full technical detail, and forgets entries after 30 minutes. A global key `D` writes the current log to a new temporary file and shows its path. The visible messages do not change.

### Problem Frame

Operators see `nie udało się wczytać przebiegu: brak połączenia z serwerem` on the *Przebieg* tab fairly often, also in local mode with the TUI and the server on the same host. Nobody can tell the cause from the screen.

The message hides the detail that the code has. Every transport failure of the client (connection refused, connection reset, request timeout, and others) becomes `ClientError::Unavailable(detail)` (`crates/narsor-api-client/src/api/client.rs:1078`, `:1086`). The TUI turns all of them into the same words and drops `detail` (`src/tui/task_app/mod.rs:1714`). A local client has a request timeout of 800 ms (`crates/narsor-api-client/src/api/client.rs:101`), so a slow answer from a server that is up also shows as "no connection". This is a hypothesis about the cause, not a verified fact; the log in this plan is how to confirm or reject it.

The TUI already has a trace log (`narsor.log` in the TUI state directory, `src/tui/paths.rs:31`), but it is written only when a log level is configured (`src/bin/narsor.rs:83`). It is off when the problem happens, and switching it on after the fact does not help with a problem that is intermittent.

### Key Decisions

- **Always on, in memory, not on disk.** The log costs nothing on disk and needs no configuration; the operator writes it out only when they need it. Governs R1, R4. (session-settled: user-directed — chosen over a file log: the request asks for memory with a dump on demand)
- **30 minutes by age.** Entries older than 30 minutes are dropped. Governs R4. (session-settled: user-directed — the request names 30 minutes)
- **Global key `D` writes to a temporary file.** Any temporary location is acceptable. Governs R6, R7. (session-settled: user-directed — the request names `D` and a temporary file)
- **Diagnosis first, visible messages unchanged.** This work collects evidence; it does not change what the screens say or how the client retries or times out. Governs R10. (assumption — see Q1)
- **Separate from the existing `narsor.log`.** The problem log does not depend on the configured log level and does not replace the trace log. Governs R1.

### Requirements

**Recording problems**

- R1. The TUI records a problem entry in memory for every failed call to the server and for every error message it shows to the operator, from start to exit, with no configuration needed.
- R2. Each entry holds: local time, the screen or tab and the operation (for example "read Przebieg of task X"), the kind of error, the full technical detail the client had (for example the text inside `ClientError::Unavailable`), and how long the call took before it failed.
- R3. Entries never contain the bearer token or other credentials.
- R4. The TUI drops entries older than 30 minutes, and the memory the log uses stays bounded also when a call fails on every refresh.

**Context of the session**

- R5. The written file starts with a short header: Narsor version, the connection (local or a server URL, never a token), the request timeout in use, the time of the dump and the time span the entries cover.

**Writing the dump**

- R6. Pressing `D` on any screen writes the current log to a new file in the system temporary directory and shows the full path of the file in a notification.
- R7. `D` does not act while a text field has focus (filter, forms, dialogs with text input); there `D` types the letter as before.
- R8. When the log is empty, `D` still writes a file with the header and a line that says no problem was recorded, so the operator knows the key worked.
- R9. When the file cannot be written, the TUI shows a notification with the reason and keeps the log in memory unchanged.

**Boundaries of behavior**

- R10. The messages the operator sees today stay as they are; the TUI does not get slower, and the key-handling loop does not wait for the file to be written longer than for an ordinary key.
- R11. The key hints of the TUI and `docs/user/tui.md` list `D`.

### Acceptance Examples

- AE1. **Covers R1, R2.** **Given** the TUI runs in local mode and the read of *Przebieg* fails with a request timeout, **when** the tab shows `brak połączenia z serwerem`, **then** the log holds an entry with the tab, the task, the kind "unavailable", the client detail that names a timeout, and a duration near 800 ms.
- AE2. **Covers R4.** **Given** an entry recorded 31 minutes ago and one recorded 5 minutes ago, **when** the operator presses `D`, **then** the file holds only the 5-minute entry.
- AE3. **Covers R6, R8.** **Given** no problem happened, **when** the operator presses `D` on the task list, **then** a file with the header and "no problems recorded" is written and its path is shown.
- AE4. **Covers R7.** **Given** the list filter `/` is open, **when** the operator types `D`, **then** the filter text gets `D` and no file is written.
- AE5. **Covers R3, R5.** **Given** a remote connection with a token, **when** the dump is written, **then** the header names the server URL and the file contains no token text.

### Scope Boundaries

- Not in this work: a fix of why the reads fail (timeout length, retry, server speed). That is a follow-up plan, based on the evidence from the dump.
- Not in this work: sending the dump anywhere, or a viewer of the log inside the TUI.
- Not in this work: the same log in the Web UI or `narsor-cli`.
- Not in this work: recording successful calls; only failures and shown errors.

### Założenia do potwierdzenia

- The work only collects evidence; the visible messages stay the same (Q1 asks about this).
- "Every problem" means every failed server call (also refusals such as 404 or 409, not only transport failures) and every error message the TUI shows. Successful calls are not recorded.
- "Any temporary file" means a new file with a unique name in the system temporary directory (`$TMPDIR`, else `/tmp`), readable only by the owner, because it can contain task names and repository URLs. Each `D` writes a new file; old files are not removed by the TUI.
- `D` is global except while a text field has focus (R7). Today `D` has no other meaning in the TUI (no binding for `Char('D')` found in `src/tui`).
- The 30-minute limit counts from the time each entry was recorded. The memory bound in R4 (for example a maximum number of entries) is a planning choice.
- The problem log is independent of the existing `narsor.log`. When a log level is configured, the problem entries may also go to `narsor.log`; planning decides.
- The dump is plain text, readable by a person and attachable to a ticket.

### Pytania otwarte dla użytkownika

- Q1. **Resolved: option 1 (evidence only), confirmed by the user on the pull request.** Does this work only collect evidence, or does it also change what the operator sees when a server call fails?
  1. (recommended) Evidence only: add the log and `D`, keep the messages; decide on a fix after the first dumps show the cause.
  2. Also show the kind of failure in the message, for example `brak połączenia z serwerem (przekroczono czas 800 ms)` vs `(połączenie odrzucone)`.
  3. Also change the cause now: give the *Przebieg* read a longer timeout in local mode, in addition to option 2.

### Outstanding Questions

**Deferred to Planning**

- Where in the code the entries are recorded (one place for all client calls vs each screen), so that R1 covers all screens without duplicates.
- The memory bound of R4 and whether repeated identical failures are collapsed into one entry with a count.
- The exact file name pattern and the text layout of the dump.

### Sources / Research

- `src/tui/task_app/mod.rs:1696` — `is_transport` and `failure_reason`: `ClientError::Unavailable(_)` becomes `brak połączenia z serwerem` and the detail is dropped.
- `src/tui/task_app/tabs/progress.rs:812` — the *Przebieg* failure line; `:327` stores the failure text.
- `crates/narsor-api-client/src/api/client.rs:17` — why the local timeout is short; `:101` `REQUEST_TIMEOUT` = 800 ms; `:106` remote timeout 10 s; `:1078` and `:1086` map every send error to `Unavailable`.
- `crates/narsor-api-client/src/api/discovery.rs:275` — local mode builds the client with `ApiClient::new` (the 800 ms timeout).
- `src/bin/narsor.rs:83` and `src/tui/paths.rs:31` — the existing `narsor.log`, written only when a log level is configured.
- `docs/user/tui.md` — the list of TUI keys to update (R11).
