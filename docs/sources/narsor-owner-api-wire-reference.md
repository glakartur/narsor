# Narsor owner HTTP API: wire reference for a JS client

Source of truth: the Rust code in `/home/artur/Projekty/narsor` at commit `44262f9`.
All statements come from the code that I read. Items that I did not verify by a run are marked **UNSURE** with a file:line.

Conventions in this document:

- `T?` = the field can be `null`. "omitted when X" = the server does not write the key (`skip_serializing_if`).
- "default" = the server (or client) accepts a missing key.
- All timestamps are RFC 3339 UTC strings (chrono `DateTime<Utc>`, for example `"2026-11-13T10:00:00.123456789Z"`).
- All enums are lowercase `snake_case` strings unless shown differently.
- Many enums have a catch-all `Unrecognized` variant on the Rust side (`#[serde(other)]`). A JS client must also accept unknown strings and unknown `type`/`kind` tags.
- Unknown JSON keys are ignored by the server, except in types marked **deny_unknown_fields** (the server answers 422/400).

---

## 1. Transport, authentication, errors

### 1.1 Base URL and modes

| Mode | `whoami.mode` | Bind | Token |
|---|---|---|---|
| trusted (local daemon) | `"trusted"` | loopback only | random UUID made at server start (the daemon writes it to its discovery record: `pid`, `addr`, `token`, `started_at`) |
| network | `"network"` | behind a TLS proxy | OIDC access token of the installation owner |

The Rust client accepts `https://…`, or `http://` for a loopback host only. A path prefix in the base URL is kept (`crates/narsor-api-client/src/api/client.rs:1276`).

### 1.2 Authentication

Every request: `Authorization: Bearer <token>`.

- Trusted mode compares the full header value with `Bearer <token>` in constant time (`crates/narsor-server/src/auth_gate.rs:153`).
- Refusals have **no body**: `401` = not authenticated (missing, wrong, expired token). `403` = authenticated but not the owner (network mode), or not reached through the trusted proxy.
- The Rust client, after a `401`, reads the token again and sends the request one more time (`client.rs:1203`). This is safe for all methods because the gate refuses before a handler runs.
- `/auth/v1/whoami` and `/auth/v1/claim` are outside the owner gate. They authenticate on their own.

### 1.3 CORS (important for a browser)

`crates/narsor-server/src/auth_gate.rs:394`, wired in `crates/narsor-server/src/api.rs:408`.

- No CORS headers at all unless the setting `cors_allowed_origins` lists the exact origin (both modes).
- Allowed methods: `GET, POST, PUT, DELETE`. Allowed request headers: `authorization`, `content-type`, `last-event-id`.
- No `expose_headers`, no credentials. Do not depend on response headers other than the CORS-safelisted ones.
- In network mode the proxy check is outside CORS: a peer that is not the trusted proxy gets a bare `403`, also for a preflight.

### 1.4 Error shape

Body type `RequestErrorResponse` (only where the table says "JSON"):

```json
{ "code": "stale_generation", "message": "human-readable English text" }
```

| Status | Body | Meaning | Rust `ClientError` |
|---|---|---|---|
| 200 / 201 / 202 | JSON | success (the client treats all three the same) | — |
| 400 | JSON `{code,message}` (usual) | malformed or invalid request | `Request{code,message}` |
| 400 | **empty** | legacy `ApiError::BadRequest` (only on `/executions/...` and `POST /tasks` task-definition errors) | client fails to parse → `Server` |
| 400 / 415 / 422 | **plain text** | axum extractor rejection: bad query string (400), JSON syntax (400), missing `Content-Type: application/json` (415), JSON with wrong shape or unknown field in a deny_unknown_fields type (422). Not `{code,message}`. | `Server` |
| 401 | empty | not authenticated | `Unauthorized` |
| 403 | empty | not owner / not operator / not via proxy | `Forbidden` |
| 404 | empty | does not exist **for this caller** (also: task of another owner, legacy routes on a non-local server) | `NotFound` |
| 409 | JSON `{code,message}` | conflict with current state | `Refused{code,message}` |
| 409 | empty | legacy `ApiError::Conflict` (only `/executions/...`) | `Conflict` |
| 500 | empty | server failure | `Server` |
| 503 | JSON `{code,message}` | something did not answer (runner relay, git remote); retry is permitted | `Degraded{code,message}` |

Client rule (`crates/narsor-shared/src/api_error.rs:43`): for 409 and 503 try to parse `{code,message}`, fall back to the bare status. For 400 the body **must** parse or it is a server error.

Known `code` values (not a complete list; grep `bad_request(`, `conflict(`, `unavailable(` in `crates/narsor-engine/src/controller_protocol` and `crates/narsor-server/src/api`):

- 400: `invalid_cursor`, `invalid_identifier`, `invalid_task_id`, `invalid_launch_spec`, `invalid_workflow`, `invalid_task_label`, `invalid_move_anchor`, `invalid_concurrency_limit`, `missing_idempotency_key`, `idempotency_key_too_long`, `empty_comment`, `comment_too_long`, `invalid_input`, `unsupported_harness`, `invalid_repository`, `branches_refused`, `workflow_paths_unavailable`, `workflow_source_unavailable`, `workflow_not_yaml`, `workflow_not_found`, `workflow_unreadable`, `workflow_not_a_file`, `workflow_too_large`, `invalid_content_type`, `invalid_material_manifest`, `invalid_material_path`, `material_too_large`, `material_digest_mismatch`, `material_incomplete`, `material_refused`, `empty_message`, `invalid_proposal`, `not_undoable`.
- 409: `stale_generation`, `idempotency_key_reused`, `illegal_command`, `action_unavailable`, `task_conflict`, `task_archived`, `task_not_queued`, `task_not_accepting_input`, `obsolete_input`, `input_already_queued`, `input_already_settled`, `comment_pending`, `operator_command_refused`, `operator_command_already_settled`, `not_admitted`, `backend_unavailable`, `repository_exists`, `not_recoverable`, `material_id_reused`, `material_quota_exceeded`, `conversation_unavailable`, `conversation_reader_starting`, `conversation_record_unreadable`, `stale_proposal`, `proposal_settled`, `execution_busy`, `already_undone`, `decision_superseded`, `execution_cancelled`.
- 503: `runner_timeout`, `runner_read_failed`, `branches_unavailable`.

### 1.5 Identifiers

Task id and backend id: 1–128 characters of `[A-Za-z0-9._-]`, first character not `.` (`crates/narsor-protocol/src/identifier.rs:24`). The TUI makes task ids as `task-<uuid v4 hex without dashes>`. Idempotency keys in the TUI are UUID v4 strings. Legacy execution ids and attempt ids: `[A-Za-z0-9_-]+` (anything else is 404 / `no_record`).

### 1.6 Idempotency and optimistic concurrency

- `idempotency_key`: client-chosen string. A retry of the same request must reuse it. The response has `replayed: true` when the key was seen before. The same key with a different command → `409 idempotency_key_reused`.
- `expected_generation`: optional `u64`. Send `TaskSummary.generation`. If the task moved on → `409 stale_generation`. The TUI always sends it for owner commands, operator commands and comments. It does not send it for input (the type has no such field).

---

## 2. Endpoint index

"TUI" column: yes = the terminal UI calls it. CLI = only the CLI calls it today.

| # | Method | Path | Success | TUI |
|---|---|---|---|---|
| A1 | GET | `/auth/v1/whoami` | 200 `WhoAmIResponse` | CLI |
| A2 | POST | `/auth/v1/claim` | 200 `OwnerView` | CLI |
| T1 | GET | `/control/v1/tasks[?archived=true\|false]` | 200 `TaskListResponse` | yes (always with `archived`) |
| T2 | POST | `/control/v1/tasks` | **201** `TaskSummary` | yes |
| T3 | GET | `/control/v1/tasks/{task_id}` | 200 `TaskSummary` | CLI |
| T4 | POST | `/control/v1/tasks/{task_id}/commands` | 200 `OwnerCommandResponse` | yes |
| T5 | POST | `/control/v1/tasks/{task_id}/move` | 200 `TaskSummary` | yes |
| T6 | POST | `/control/v1/tasks/{task_id}/input` | **202** `SubmitInputResponse` | yes |
| T7 | POST | `/control/v1/tasks/{task_id}/operator-commands` | **202** `SubmitOperatorCommandResponse` | yes |
| T8 | POST | `/control/v1/tasks/{task_id}/comment` | **202** `SubmitCommentResponse` | yes |
| T9 | GET | `/control/v1/tasks/{task_id}/events?after=N` | 200 `TaskEvent[]` | CLI |
| T10 | GET | `/control/v1/tasks/{task_id}/timeline` | 200 `TimelineResponse` | yes |
| T11 | GET | `/control/v1/tasks/{task_id}/conversation` | 200 `RemoteConversationResponse` | CLI |
| T12 | GET | `/control/v1/tasks/{task_id}/attempts/{attempt_id}/conversation?from=N&byte_budget=B` | 200 `ConversationPage` | yes |
| T13 | GET | `/control/v1/tasks/{task_id}/execution-events?after=N&limit=L` | 200 `ExecutionEventsResponse` | yes |
| T14 | GET | `/control/v1/tasks/{task_id}/conductor?after=N&limit=L` | 200 `ConductorReadResponse` | yes |
| S1 | GET | `/control/v1/events/stream?cursor=C&task_id=ID` | 200 `text/event-stream` | yes |
| M1 | GET | `/control/v1/tasks/{task_id}/materials` | 200 `MaterialListResponse` | CLI |
| M2 | POST | `/control/v1/tasks/{task_id}/materials` | **201** (or 200 when replayed) `MaterialUploadResponse` | CLI |
| M3 | GET | `/control/v1/tasks/{task_id}/materials/{material_id}` | 200 `MaterialView` | CLI |
| M4 | DELETE | `/control/v1/tasks/{task_id}/materials/{material_id}` | 200 `MaterialSummary` | CLI |
| M5 | GET | `/control/v1/tasks/{task_id}/materials/{material_id}/blobs/{sha256}` | 200 `application/octet-stream` | CLI |
| C1 | GET | `/control/v1/capabilities` | 200 `CapabilitiesView` | yes |
| C2 | GET | `/control/v1/default-backend` | 200 `DefaultBackendView` | CLI |
| C3 | PUT | `/control/v1/default-backend` | 200 `DefaultBackendView` | CLI |
| C4 | GET | `/control/v1/concurrency-limit` | 200 `ConcurrencyLimitView` | yes |
| C5 | PUT | `/control/v1/concurrency-limit` | 200 `ConcurrencyLimitView` | yes |
| C6 | GET | `/control/v1/usage` | 200 `HarnessUsageResponse` | yes |
| C7 | GET / PUT / DELETE | `/settings/agent-harness` | 200 `HarnessSelectionSnapshot` | yes |
| W1 | GET | `/control/v1/workflows` | 200 `WorkflowListResponse` | yes |
| W2 | POST | `/control/v1/workflows/source` | 200 `WorkflowSourceResponse` | CLI |
| W3 | POST | `/control/v1/workflows/validate` | 200 `ValidateWorkflowResponse` | yes |
| W4 | POST | `/tasks` | **202** `StartTaskResponse` | yes (workflow given as a file path) |
| R1 | GET | `/control/v1/repositories` | 200 `RepositoryListResponse` | yes |
| R2 | POST | `/control/v1/repositories` | **201** `RepositoryView` | yes |
| R3 | DELETE | `/control/v1/repositories/{id}` | 200 `RepositoryView` | yes |
| R4 | GET | `/control/v1/repositories/{id}/branches` | 200 `RepositoryBranchesResponse` | yes |
| H1 | GET | `/control/v1/executions` | 200 `LegacyExecutionListResponse` | yes |
| H2 | GET | `/control/v1/executions/{execution_id}` | 200 `LegacyExecutionView` | CLI |
| H3 | GET | `/control/v1/executions/{execution_id}/events?after=N` | 200 `Event[]` | yes |
| H4 | GET | `/control/v1/executions/{execution_id}/timeline` | 200 `Timeline` | yes |
| H5 | GET | `/control/v1/executions/{execution_id}/attempts/{attempt_id}/conversation?from=N&byte_budget=B` | 200 `ConversationPage` | yes |
| H6 | GET | `/control/v1/executions/{execution_id}/conductor` | 200 `ConductorStateResponse` | yes |
| H7 | GET | `/control/v1/executions/{execution_id}/conductor/chat?after=N` | 200 `ConductorChatResponse` | yes |
| H8 | POST | `/control/v1/executions/{execution_id}/recover` | **202** `AcceptedResponse` | yes |

The `/control/v1/...` routes exist only when the server has a control plane (the normal daemon has one). `/control/v1/backends/.../enrollments` and `/control/v1/controllers...` are operator routes for controllers. The TUI does not use them. They are not in this document.

**Conductor chat / confirm / reject / undo for a task** have no endpoint of their own. The TUI sends them as operator commands through T7 (`conductor_chat`, `confirm_proposal`, `reject_proposal`, `undo_decision`). The routes `/executions/{id}/conductor/chat` (POST), `/conductor/proposals/{id}/confirm|reject`, `/conductor/decisions/{id}/undo` are the legacy path for pre-scheduler executions. Only the CLI uses them (see section 12).

---

## 3. Identity

### A1 `GET /auth/v1/whoami`

Response 200 `WhoAmIResponse`:

```json
{ "mode": "trusted", "issuer": null, "subject": null, "owner": true, "claimed": true, "expires_at": null }
```

| Field | Type | Notes |
|---|---|---|
| `mode` | `"trusted"` \| `"network"` | |
| `issuer`, `subject` | string? | `null` in trusted mode |
| `owner` | bool | the caller is the installation owner |
| `claimed` | bool | the installation has an owner |
| `expires_at` | timestamp? | token expiry; `null` in trusted mode |

Errors: bare `401`. In network mode a valid token of a non-owner gets `200` with `owner:false` (this route does not answer 403).

### A2 `POST /auth/v1/claim`

Body: `{ "claim_code": "<one-time code printed by the daemon>" }` (the server trims it; body limit 64 KiB).

Response 200 `OwnerView`:

```json
{ "issuer": "https://idp", "subject": "abc", "bound_by": "claim", "bound_at": "…", "revision": 1 }
```

`bound_by`: `"configuration"` \| `"claim"`. A second claim by the owner answers 200 with the same record.

Errors (all bare): `404` trusted mode (nothing to claim), `401` bad token, `400` unreadable body, `403` wrong or spent code or owned by somebody else, `500`.

---

## 4. Tasks

### T1 `GET /control/v1/tasks`

Query: `archived` (optional, `true`/`false`). `true` = only archiving/archived tasks, newest archive first. `false` = only the others, in queue order. Absent = all.

Response 200 `TaskListResponse`:

```json
{ "tasks": [ TaskSummary, … ], "cursor": "task-a:5,task-b:3" }
```

`cursor` is the stream position over **all** tasks of the owner, whatever the filter. Open the stream (S1) with it. It is `""` when the owner has no task.

### T2 `POST /control/v1/tasks`

Body `CreateTaskRequest`:

```json
{
  "task_id": "task-3f2a…",            // optional, default null; send it so a retry is idempotent
  "backend_id": "local",               // optional; owner default when absent
  "launch": {                          // deny_unknown_fields
    "workflow_id": "feature",          // workflow name from W1
    "workflow_version": 3,             // WorkflowView.version
    "inputs": { "ticket": "ABC-1" },   // default null; object of workflow inputs
    "workspaces": {                    // optional; by workspace name; values deny_unknown_fields
      "main": { "repository": "repo-id", "branch": "develop", "commit": null }
    },
    "profile": null,                   // optional string: environment profile
    "harness": null,                   // optional string: harness id
    "resources": { "cpu_millicores": 1000, "memory_mib": 2048 },  // optional
    "ticket": "ABC-1",                 // optional; server trims and validates
    "title": "My task"                 // optional; server trims and validates (TUI caps at 50 chars)
  }
}
```

The TUI sends `task_id`, `backend_id: null`, `profile/harness/resources: null`.

Response **201** `TaskSummary` (also for a replay of the identical request).
Errors: `400 invalid_identifier | invalid_launch_spec | invalid_task_label | <launch error code>`, `409 task_conflict` (same id, different request), `409 not_admitted | backend_unavailable`, `404` (id taken by another owner).

### T3 `GET /control/v1/tasks/{task_id}` → 200 `TaskSummary`. `404` when unknown or not owned.

### T4 `POST /control/v1/tasks/{task_id}/commands`

Body `OwnerCommandRequest`:

```json
{ "command": "pause", "idempotency_key": "uuid", "expected_generation": 7 }
```

`command` (`OwnerCommand`): `"start"` (retry an interrupted/unknown task), `"pause"`, `"release_slot"` (free the slot of a task that waits for the owner), `"resume"`, `"archive"` (delete workload and storage; cannot be undone).

Response 200 `OwnerCommandResponse`:

```json
{
  "receipt": {
    "idempotency_key": "uuid", "command": "pause", "expected_generation": 7,
    "accepted_at": "…", "generation_after": 8, "runner_state_after": "pause_requested",
    "revision": 12, "actor": "owner"
  },
  "replayed": false,
  "task": TaskSummary,
  "queued_for_slot": false
}
```

`actor`: `"owner"` \| `"policy"`. `queued_for_slot: true` = a resume found the backend full; the task waits at the front of the queue.
Errors: `409 illegal_command | stale_generation | idempotency_key_reused | task_archived | action_unavailable`, `404`.

### T5 `POST /control/v1/tasks/{task_id}/move`

Body `MoveTaskRequest` (adjacently tagged target: tag `to`, content `task_id`):

```json
{ "target": { "to": "before", "task_id": "task-b" }, "expected_generation": 7 }
{ "target": { "to": "after",  "task_id": "task-b" } }
{ "target": { "to": "top" } }
{ "target": { "to": "bottom" } }
```

Only a queued task can move. A repeat of a move that already holds is a no-op. Response 200 `TaskSummary`.
Errors: `409 task_not_queued | stale_generation`, `400 invalid_move_anchor | invalid_identifier`, `404`.

### T6 `POST /control/v1/tasks/{task_id}/input`

Body `SubmitInputRequest`:

```json
{
  "pending_input_id": "<TaskSummary.runner.pending_input.id>",
  "idempotency_key": "uuid",
  "delivery": HumanInputDeliveryDto
}
```

Response **202** `SubmitInputResponse`: `{ "input": InputView, "replayed": false }`. Delivery and consumption come later (task events `inputs_updated`, and `TaskSummary.inputs[]`).
If the task is paused, the server resumes it for the work (or queues it for a slot).
Errors: `400 missing_idempotency_key | invalid_input`, `409 obsolete_input | input_already_queued | input_already_settled | task_not_accepting_input | idempotency_key_reused`, `404`.

### T7 `POST /control/v1/tasks/{task_id}/operator-commands`

Body `SubmitOperatorCommandRequest`. The command is **flattened** into the body (tag `kind`):

```json
{ "kind": "interrupt", "idempotency_key": "uuid", "expected_generation": 7 }
{ "kind": "stop_step", "idempotency_key": "uuid" }
{ "kind": "transition", "target": "review", "override": false, "reason": null, "idempotency_key": "uuid" }
{ "kind": "select_step", "target": "work", "note": "fix the tests", "idempotency_key": "uuid" }
{ "kind": "restart_step", "note": null, "failure_key": "<episode>", "idempotency_key": "uuid" }
{ "kind": "conductor_chat", "text": "why did it fail?", "idempotency_key": "uuid" }
{ "kind": "confirm_proposal", "proposal_id": "p-1", "idempotency_key": "uuid" }
{ "kind": "reject_proposal", "proposal_id": "p-1", "idempotency_key": "uuid" }
{ "kind": "undo_decision", "decision_id": "d-1", "idempotency_key": "uuid" }
```

Notes: the Rust field `forced` is `override` on the wire. `set_conductor_harness {harness}` exists only so old records decode; do not send it.

Response **202** `SubmitOperatorCommandResponse`: `{ "command": OperatorCommandView, "replayed": false }`. The verdict comes later: task event `operator_command_resolved` and `TaskSummary.operator_commands[]` (`status` `applied`/`rejected`, `detail`).
Errors: `409 operator_command_refused | stale_generation | idempotency_key_reused | action_unavailable`, `404`.

### T8 `POST /control/v1/tasks/{task_id}/comment`

Body `SubmitCommentRequest`: `{ "text": "…", "idempotency_key": "uuid", "expected_generation": 7 }`.
Limits: text 1–4000 characters after trim; key 1–128 characters.
The server chooses the effect from the way the task is stopped (answer to a question, message for the next launch, …).

Response **202** `SubmitCommentResponse`: `{ "task": TaskSummary, "replayed": false }`.
Errors: `400 missing_idempotency_key | idempotency_key_too_long | empty_comment | comment_too_long`, `409 stale_generation | comment_pending | action_unavailable`, `404`.

### T9 `GET /control/v1/tasks/{task_id}/events?after=N`

`after`: u64, default 0. Returns control-plane `TaskEvent[]` with `sequence > after` (no page limit). These are **not** the workflow events (see T13).

---

## 5. Task DTOs

### 5.1 `TaskSummary`

```json
{
  "task_id": "task-3f2a",
  "backend_id": "local",
  "runner_state": "running",
  "state": "human_needed",
  "holds_slot": true,
  "generation": 7,
  "revision": 42,
  "rank": 0,
  "created_at": "…",
  "state_since": "…",                 // omitted when unknown
  "queue_sequence": 3,
  "workflow_id": "feature",
  "workflow_version": 3,
  "launch_digest": "sha256…",
  "environment_identity": "…",
  "archive": null,                     // ArchiveTombstone?  (always present)
  "observation": null,                 // ObservationView?   (always present)
  "runner": null,                      // RunnerView?        (always present)
  "controller": null,                  // ControllerPresence? (always present)
  "inputs": [ InputView ],
  "operator_commands": [ OperatorCommandView ],
  "attention": [ AttentionItem ],
  "queue": null,                       // QueueView?
  "workspaces": [ WorkspaceHeader ],
  "ticket": "ABC-1",                   // omitted when none
  "title": "…",                        // omitted when none (operator's, else generated)
  "manual_transitions": TransitionOffer,  // omitted when none
  "slot_release_pending": false,
  "steps": [ StepView ],               // omitted when the owner cannot choose a step
  "actions": [ TaskActionOffer ],      // omitted by an old server
  "panel": PanelOffer,                 // omitted by an old server
  "resume": ResumeState,
  "as_of": "…"
}
```

Enums:

- `state` (`TaskState`, what the owner sees; the server computes it, the client only shows it): `queued`, `running`, `human_needed`, `paused`, `finished`, `error`, `archived` (+ unknown → treat as unrecognized).
- `runner_state` (`RunnerState`, resources): `queued`, `provisioning`, `running`, `pause_requested`, `stopped`, `interrupted`, `unknown`, `archiving`, `archived`. Old records spell `stopped` as `paused` and the key `runner_state` as `lifecycle` (the server writes the new names).
- `revision` equals the sequence of the task's last event (the list cursor is built from it, `service/tasks/lookup.rs:39`).

Sub-objects:

```jsonc
// QueueView — note the nested tag: QueueReason is tagged with the key "reason"
{ "reason": { "reason": "waiting_for_capacity", "position": 2, "running": 3, "limit": 3 },
  "message": "waiting for capacity: position 2 in the queue, 3 of 3 slots in use",
  "since": "…" }
// other QueueReason values:
{ "reason": "admitted" }
{ "reason": "controller_offline" }
{ "reason": "backend_unknown", "detail": "…" }

// AttentionItem
{ "kind": "question", "state_id": "review", "since": "…", "waiting_id": "…" }
// kind: question | approval | problem | entry_limit | failed_step | conductor_proposal | provisioning_failed

// InputView
{ "input_id": "…", "pending_input_id": "…", "status": "queued", "submitted_at": "…", "updated_at": "…", "detail": null }
// status: queued | delivered | consumed | rejected | obsolete      (settled = consumed/rejected/obsolete)

// OperatorCommandView
{ "command_id": "…", "command": { "kind": "interrupt" }, "status": "queued", "submitted_at": "…", "updated_at": "…", "detail": null }
// status: queued | applied | rejected.   `command` is NOT flattened here.

// ArchiveTombstone
{ "idempotency_key": "…", "generation": 9, "requested_at": "…", "completed_at": null, "resources_never_created": false }

// ControllerPresence
{ "controller_id": "…", "lease_epoch": 4, "lease_expires_at": "…", "last_heartbeat_at": null, "freshness": "live" }
// freshness: live | stale

// ObservationView
{ "observation": { "kind": "runner_ready" },
  "summary": { "workload_present": true, "storage_present": true, "runner_status": "running", "detail": null },
  "resource": { "uid": "…", "resource_generation": 1 },   // or null
  "observed_at": "…", "received_at": "…", "freshness": "live" }
// observation.kind: start_accepted | runner_ready | paused_at_safe_point | workload_lost {state_known:bool}
//                   | loss_confirmed | resources_deleted | status

// RunnerView
{ "incarnation": "…",
  "execution_status": "input_needed",
  "execution_cursor": { "execution_id": "…", "current_state": "review", "action_execution_id": "…",
                        "current_action_type": "agent", "current_decision_by": null },
  "event_cursor": 120,
  "pending_input": { "id": "<episode key>", "attempt_id": "…", "form": PendingHumanInputView },  // or null
  "safe_point": false,
  "problem": { "code": "…", "message": "…" },            // or null
  "conductor_available": true,
  "conductor_unavailable_reason": null,
  "default_harness_unavailable": { "harness": "codex", "kept": "claude-code", "reason": "…" },  // or null
  "observed_at": "…", "received_at": "…", "last_contact_at": "…", "freshness": "live" }

// ResumeState (every key has a default)
{ "consecutive_losses": 0, "next_attempt_at": null,
  "last_loss": { "from": "running", "at": "…", "lease_epoch": 4, "reason": null },
  "exhausted": { "reason": "could not resume automatically: …", "at": "…", "announced": false },
  "uncounted_losses": 0, "last_loss_uncounted": false }

// WorkspaceHeader (name and repo default ""; other keys omitted when unknown)
{ "name": "main", "repo": "narsor", "branch": "feat/x", "base_branch": "main",
  "added": 120, "removed": 4, "pr": "42", "pr_url": "https://…" }

// TransitionOffer
{ "from_state": "review", "exits": [ { "label": "approve", "target": "merge", "description": "…" } ],
  "override_allowed": true, "override_targets": ["work", "done"] }

// StepView
{ "id": "work", "label": "Work", "kind": "agent" }
// kind: agent | profile | script | human | wait | decision | terminal
```

`ExecutionStatusDto` (used in many places): `running`, `paused`, `input_needed`, `waiting_for_event`, `completed`, `failed`, `cancelled`, `unknown`. Terminal = completed/failed/cancelled.

### 5.2 `TaskActionOffer` (what the owner can do now; the server describes it completely)

```json
{
  "id": "select_step",
  "available": true,
  "reason": "…",              // omitted when absent
  "effect": "…",              // omitted when absent
  "options": [ { "label": "…", "description": "…" } ],   // omitted when empty
  "label": "Wybierz krok",    // omitted by a server that does not describe actions → nothing to draw
  "description": "…",         // omitted when absent
  "irreversible": true,       // omitted when false
  "order": 40,                // lower first
  "kind": "submenu",          // execute (default) | submenu
  "surface": "panel",         // panel (default) | conversation
  "fields": [ TaskActionField ],        // omitted when empty; for a leaf only
  "children": [ TaskActionOffer ],      // omitted when empty; same shape, same id as the parent
  "send": TaskActionSend                // omitted for a submenu and for client-specific actions (an answer)
}
```

- `id` (`TaskActionId`): `comment`, `answer`, `approve`, `reject`, `allow_permission`, `deny_permission`, `pause`, `resume`, `interrupt`, `transition`, `restart`, `archive`, `stop`, `release_slot`, `select_step`.
- `TaskActionField`: `{ "name": "komentarz", "kind": "multiline", "required": true, "hint": "…", "options": ["a","b"] }`. `kind`: `text` | `multiline` | `select`. `required` omitted when false; `hint`, `options` omitted when empty.
- `TaskActionSend` (tag `type`):

```json
{ "type": "owner",    "command": "pause" }                                  // POST …/commands  with this OwnerCommand
{ "type": "operator", "command": { "kind": "select_step", "target": "work", "note": "{{komentarz}}" } }  // POST …/operator-commands
{ "type": "comment",  "field": "tekst" }                                    // POST …/comment with text = value of that field
```

Placeholder rule (`controller/task/actions.rs:269-345`): a string in `command` that is exactly `{{<field name>}}` stands for the trimmed value of that field. A blank field becomes `null`. Replace recursively in arrays and objects. For `comment`, a blank field is an error on the client side. An unknown `type` or an unknown command cannot be sent ("needs a newer client").

### 5.3 `PanelOffer` (the forms of the owner's panel)

```json
{ "forms": [ PanelForm ], "default": 0 }      // default: index of the form that opens first, or null
```

```json
{
  "kind": "answer",                 // action | answer | chat | entry_limit | permission
  "waiting_id": "…",                // omitted for a form that answers no wait
  "choices": [ { "label": "approve", "description": "…", "action": "transition" } ],  // omitted when empty
  "definition": PanelDefinition     // omitted for the chat form and the manual-action form
}
```

A form with `kind:"action"` and no `waiting_id` is the form of the task's own actions (manual transition). With a `waiting_id` it is the output choice of a wait.

`PanelDefinition` (`crates/narsor-protocol/src/panel/mod.rs:274`):

```json
{
  "continuation": "resume_agent",
  "episode": "<pending input id>",
  "context": [ { "tone": "plain", "text": "…" } ],     // tone: plain | dim | conductor | problem
  "entries": [ PanelEntry ],
  "composer": { "entry": 0, "label": "…", "help": "…" }   // omitted when absent
}
```

`PanelEntry`:

```json
{
  "label": "approve",
  "description": "",
  "fields": [ PanelField ],          // omitted when empty
  "resumes": "work",                 // omitted when absent
  "resumes_attempt": "…",            // omitted when absent
  "recommended": false,
  "plan": PanelPlan,
  "summary": "text for the notification after send"
}
```

`PanelField`: `{ "name": "answer", "kind": PanelFieldKind, "required": false, "description": "…", "value": "start text", "proposed": 3 }` (`description`, `value`, `proposed` omitted when absent).

**`PanelFieldKind` and `PanelPlan` are externally tagged enums** (no `tag` attribute): a unit variant is a bare string, a struct variant is a one-key object.

```json
// PanelFieldKind
{ "text": { "multiline": true } }
{ "select": { "options": ["a", "b"] } }

// PanelPlan
{ "complete": { "label": "approve", "label_field": "decision" } }   // both can be null
"answer"
{ "output": { "label": "done" } }
"message"
"approve"
"return"
{ "permission": { "decision": PermissionDecisionDto } }
{ "entry_limit": { "decision": "continue" } }                       // "continue" | "reset_counter"
```

How a chosen entry becomes the `delivery` of T6 (`panel/mod.rs:324`, function `deliver`). `values` = list of (field name, typed text). Always set `delivery.episode = definition.episode`.

| `plan` | `delivery` |
|---|---|
| `complete{label,label_field}` | `values` = every field as a string; if both `label_field` and `label` are set, also `values[label_field] = label`; `chosen_label = label` |
| `answer` | `values` = every field as a string. At least one of the fields `answer` or `option` must be present |
| `output{label}` | `chosen_label = label`; fields named `artifacts.<k>` go to `artifacts[k]` (string), fields named `variables.<k>` go to `variables[k]` (the untouched `proposed` JSON value, else the typed string); field `variables (klucz=wartość)` = extra lines `key=value` added to `variables`; field `komentarz` → `comment` (trimmed) |
| `message` | `agent_message` = value of field `Wiadomość dla agenta` |
| `approve` | `action = "approve"` |
| `return` | `action = "resume"`, `comment` = value of field `Komentarz dla agenta` |
| `permission{decision}` | `permission_decision` = `decision` with `pattern` = trimmed value of field `Wzorzec polecenia` (only when `decision.scope == "pattern"`) and `message` = trimmed non-empty value of field `Wiadomość dla agenta` |
| `entry_limit{decision}` | `values = { "entry_limit_decision": decision }` |

The field names above are literal Polish strings that the server puts in the definition. Use the names from the definition; do not translate them.

### 5.4 `HumanInputDeliveryDto` (the answer in T6)

Every key has a default; send only what you need.

```json
{
  "values": { "answer": "yes" },          // object, default {}
  "chosen_label": "approve",              // string?
  "artifacts": { "report": "out/r.md" },  // object string→string, default {}
  "variables": { "count": 3 },            // object, default {}
  "comment": "…",                         // string?
  "agent_message": "…",                   // string?
  "action": "approve",                    // "approve" | "resume" | "start_new_session" | null
  "episode": "<pending input id>",        // string?
  "permission_decision": {                // PermissionDecisionDto?
    "denial_index": 0,                    // omit = decide every denial
    "decision": "allow",                  // "allow" | "deny"
    "scope": "exact_operation",           // "exact_operation" | "command_group" | "pattern" | null
    "pattern": "npm run *",               // only for scope "pattern"
    "message": "…"
  }
}
```

### 5.5 `PendingHumanInputView` (`TaskSummary.runner.pending_input.form`)

The server stores the engine record as JSON. The client reads this view of it (extra keys can be present; ignore them).

```json
{
  "execution_id": "…", "state_id": "review", "action_execution_id": "…",
  "instruction": "…",
  "fields": [ { "name": "decision", "type": "select", "options": ["a","b"], "required": true, "description": null } ],
  "reason": "…",                          // string?
  "required_values": [ { "name": "plan", "kind": "artifact", "value": "…" } ],   // kind: variable | artifact
  "answer_tail": "…", "agent_harness": "claude-code", "agent_session_id": "…", "prefilled_answer": null,
  "permission": { "denials": [ { "tool_name": "Bash", "tool_use_id": "…", "input": {…}, "cwd": "/w", "reason_type": "asyncAgent", "reason": "…" } ] },
  "outputs": [ { "label": "approve", "description": "…", "requires_human_approval": false,
                 "resume_comment_field": "…", "requires_variables": ["x"] } ],
  "continuation": "complete_step",
  "episode": "…",
  "problem": { "reason": "…",
               "answers": [ { "action_execution_id": "…", "reason": "…", "answer_tail": null } ],
               "requirements": [ { "label": "done", "variables": ["x"], "artifacts": ["report"] } ],
               "analysis": { "label": "done", "artifacts": {}, "variables": {}, "rationale": "…" } },
  "entry_limit": { "entries": 5, "max_entries": 5 },
  "approval": { "action_execution_id": "…",
                "result": { "output_label": "done", "reason": "", "data": null, "variables": {}, "artifacts": {},
                            "exit_code": 0, "stdout": "", "stderr": "" },
                "artifacts": [ { "id": "…", "artifact_type": "…", "location": "…", "name": null, "metadata": null,
                                 "producer": "…", "created_at": "…", "size_bytes": 10 } ],
                "agent_harness": null, "agent_session_id": null },
  "resume_targets": { "<label>": { "action_execution_id": "…", "harness": "claude-code", "session_id": "…" } },
  "local_values": [ { "name": "n", "value": "…" } ],
  "label_from_menu": false,
  "requested_at": "…"
}
```

- `fields[].type`: `text` | `select`.
- `continuation` (`InputContinuationView`): `complete_step` (default), `resume_agent`, `resolve_output`, `approve_output`, `resolve_permission`, `entry_limit`.
- `outputs` items can also be bare strings in old records (`"approve"` = `{label:"approve"}`).
- The id that an answer names is `episode` if not empty, else `action_execution_id`. It equals `runner.pending_input.id`.
- A permission denial can be approved only when `reason_type` is `asyncAgent` or `subcommandResults`.

Prefer `panel.forms[].definition` over building a form from this record. The definition already has the rules.

---

## 6. Event stream (S1)

`GET /control/v1/events/stream`

| Query | Type | Meaning |
|---|---|---|
| `cursor` | string, optional | resume after this cursor. Wins over `Last-Event-ID`. |
| `task_id` | string, optional | only this task's events. Unknown or not owned → bare `404`. |

Request header `Last-Event-ID: <cursor>` is used when `cursor` is absent. With neither, the stream replays **every** event of every task from the start.

Errors before the stream opens: `400 {code:"invalid_cursor"}`, `404`, `401`, `403`.

Response: `200`, `Content-Type: text/event-stream`. Frames (axum `Sse`, `crates/narsor-server/src/controller_protocol/http.rs:689`):

```
event: task
id: task-a:5,task-b:3
data: {"cursor":"task-a:5,task-b:3","event":{"sequence":5,"task_id":"task-a","at":"…","runner_state":"running","generation":7,"kind":{"type":"queue_changed","reason":{"reason":"admitted"}}}}

: keep-alive

```

- One event name only: `task`. `data` is one line of JSON: `ControlEventEnvelope`.
- `id` equals `data.cursor`: the cursor **after** this event.
- Keep-alive: an SSE comment line every 15 s when idle. **UNSURE** about the exact bytes (`: keep-alive` or `:keep-alive`); any line that starts with `:` is a comment. See axum `KeepAlive::text` used at `http.rs:683`.
- The Rust client splits frames at `\n\n` and reads only lines with the prefix `data: ` (`client.rs` function `extract_sse_events`). It ignores `event:` and `id:` and takes the cursor from the JSON.
- The stream never ends on content. It ends when: the token expires (network mode ends the stream at that moment), the caller stops being the owner (checked each second), a proxy closes it, or the server stops.

Cursor format (`crates/narsor-engine/src/controller_protocol/wire.rs:207`): `<task_id>:<sequence>` pairs joined by `,`, sorted by task id. Example `task-a:5,task-b:3`. Empty string = start of the stream. Per task: the last sequence seen. A task that is missing from the cursor is replayed from sequence 1. Treat the cursor as opaque; store the last one and send it back. URL-encode it (`:` and `,`).

Delivery guarantee: each event exactly once per cursor, each task's events in order, tasks merged by event time `at`.

Recommended start-up (what the TUI does): `GET /control/v1/tasks?archived=false` → keep `cursor` → open the stream with `?cursor=<cursor>` → on each event apply `event.kind` or read the task again → keep `envelope.cursor`.

Reconnect policy of the Rust client (`client.rs:1128-1387`): after the server ends the stream, reconnect from the last cursor. Immediately if the connection delivered at least one event; else with a delay 250 ms × 2^(n-1), max 15 s. Transport errors and 5xx: retry with the same delay. `401` after a token refresh, `403`, `404`, `400` (invalid cursor): stop.

Browser note: `EventSource` cannot send an `Authorization` header. Use `fetch` with a streamed body and parse the frames yourself.

### 6.1 `ControlEventEnvelope` and `TaskEvent`

```json
{ "cursor": "task-a:5",
  "event": { "sequence": 5, "task_id": "task-a", "at": "…", "runner_state": "running", "generation": 7,
             "kind": { "type": "…", … } } }
```

`sequence` is per task, gapless, starts at 1. `kind` is tagged with `type` (`TaskEventKind`):

```jsonc
{ "type": "created" }
{ "type": "owner_command_accepted", "command": "pause", "idempotency_key": "…" }
{ "type": "controller_reported", "observation": { "kind": "runner_ready" }, "lease_epoch": 4 }
{ "type": "runner_reported", "incarnation": "…", "event_cursor": 120, "execution_status": "running",
  "pending_input_id": null, "attention": [ AttentionItem ] }
{ "type": "input_queued", "input_id": "…", "pending_input_id": "…" }
{ "type": "resume_message_stored", "idempotency_key": "…" }
{ "type": "resume_message_taken", "idempotency_key": "…" }
{ "type": "operator_command_queued", "command_id": "…", "command": { "kind": "interrupt" }, "requested_by": "local" }
{ "type": "operator_command_resolved", "command_id": "…", "status": "applied", "detail": null }
{ "type": "priority_changed", "priority": 0 }
{ "type": "moved", "rank": 2, "order": ["task-b", "task-a"] }
{ "type": "queue_changed", "reason": { "reason": "admitted" } }
{ "type": "inputs_updated", "updates": [ { "input_id": "…", "status": "consumed" } ] }
{ "type": "material_changed", "material_id": "…", "change": "uploaded", "files": 2, "total_bytes": 1024 }
{ "type": "auto_resumed", "attempt": 1, "limit": 5, "loss_reason": null }
{ "type": "auto_resume_exhausted", "attempts": 5, "reason": "…" }
{ "type": "slot_release_scheduled", "due_at": "…" }
{ "type": "slot_released", "reason": "grace_elapsed" }     // terminal | grace_elapsed | limit_lowered
{ "type": "limit_stop_withdrawn" }
{ "type": "reader_leased", "until": "…" }
{ "type": "reader_lease_ended" }
```

`material_changed.change`: `uploaded`, `published`, `delivered`, `deleted`, `expired`. `requested_by` is a plain string (owner reference; `"local"` in trusted mode). Unknown `type` → ignore the content, still advance the cursor.

A `TaskEvent` does not carry the `TaskSummary`. Read T3 (or T1) to get the new summary.

---

## 7. Task reads: timeline, conversation, workflow events, conductor

### T10 `GET /control/v1/tasks/{task_id}/timeline` → 200 `TimelineResponse`

```json
{ "task_id": "task-a", "freshness": "live", "observed_at": "…", "timeline": Timeline,
  "execution_status": "running", "current_state": "work", "current_position": "work #1" }
```

`freshness`: `live` | `stale`. `observed_at`, `timeline`, `execution_status`, `current_state`, `current_position` are `null` when the runner did not report yet. This is the **last timeline the runner reported**, served from the control plane (no relay, fast).

`Timeline`:

```json
{ "positions": [ Position ],
  "links": { "17": { "link": "position", "value": "work #1" },
             "18": { "link": "no_link", "value": { "reason": "…" } } },
  "default_position": "work #1" }
```

`links` keys are workflow event sequences as **strings** (JSON object keys). `EventLink` is adjacently tagged (`link`, `value`).

`Position`:

```json
{
  "label": "work #1",                 // unique; "<state> #<session_number>" plus ".<k>" for later entries of one session
  "state_id": "work", "visit": 1,
  "action_execution_id": "…",         // the latest attempt; string?
  "action_type": "agent",             // string?
  "decision_by": null,                // "conductor" | "human" | null
  "status": "in_progress",            // succeeded | failed | interrupted | in_progress | awaiting_decision
  "started_at": "…", "ended_at": null, "error": null,
  "resumed_harness_session": null, "harness_session_interrupted": false, "retry_pending": false,
  "conversation": { "state": "recorded" },   // or {"state":"not_created","reason":"…"} or {"state":"no_record"}
  "session_number": 1,
  "attempts": ["a1", "a2"],           // omitted when empty → use [action_execution_id]
  "harness": "claude-code", "model": "…", "effort": "high", "harness_session_id": "…",
  "usage": { "total": TokenCounts, "subagents": TokenCounts, "subagent_count": 2,
             "subagent_stats": { "spawned": 2, "completed": 2, "failed": 0, "killed": 0, "refused": 0, "tokens": 0 },
             "cost_usd": 0.42, "conductor_total": TokenCounts, "conductor_cost_usd": 0.01 }
}
```

`TokenCounts`: `{ "input": 0, "output": 0, "cache_read": 0, "cache_write": 0 }`. In `usage`, a `null`/missing part means "the harness did not say", not zero. `subagent_stats`, `conductor_total`, `conductor_cost_usd` are omitted when absent.

Attempt ids for T12 come from `Position.attempts` (or `action_execution_id`).

### T11 `GET /control/v1/tasks/{task_id}/conversation` → 200, tagged with `status`

```json
{ "status": "available", "attempts": [ { "attempt_id": "…", "state_id": "work", "entries": 120, "closed": false, "last_entry_at": "…" } ],
  "observed_at": "…", "source": "runner", "recorded_at": "…" }        // recorded_at omitted when absent
{ "status": "unavailable", "reason": "paused", "last_attempts": [ … ], "observed_at": "…", "message": "…" }   // message omitted when absent
```

`reason` (`ConversationUnavailableReason`): `not_started`, `paused`, `interrupted`, `archived`, `runner_unreachable`, `runner_reported`, `storage_unreadable`, `reader_starting`. `source`: `runner` | `storage`. Summary only, no transcript content. The TUI does not call this; it takes attempt ids from the timeline.

### T12 `GET /control/v1/tasks/{task_id}/attempts/{attempt_id}/conversation`

| Query | Type | Default | Meaning |
|---|---|---|---|
| `from` | u64 | 0 | first entry number wanted (inclusive) |
| `byte_budget` | u64 | 262144 | soft cap on raw bytes; server clamps to 1…1048576. The TUI sends 65536. |

Relayed through the live runner: the request waits for the runner's next poll, up to 20 s on the server (config default, `controller_protocol/service.rs:128`). The Rust client allows 60 s; the TUI gives up after 3 s and asks again later. For a stopped task the control plane starts a read-only runner.

Response 200 `ConversationPage`, tagged with `status`:

```json
{ "status": "recorded",
  "entries": [ ConversationEntryDto ],
  "next_entry": 121,          // pass as `from` for the next page
  "closed": false,            // the attempt's record is closed (no more entries will come)
  "truncated_tail": false,
  "source": "runner",         // omitted when absent: "runner" | "storage"
  "recorded_at": "…" }        // omitted when absent
{ "status": "no_record" }     // a normal answer: script step, conversation never started, old log
```

Paging rule of the TUI (`src/tui/task_app/tabs/conversation.rs:141`): stop when the page is empty, or `closed`, or `truncated_tail`, or `next_entry <= from`. Else ask again with `from = next_entry`. For a live attempt, poll later from the stored `next_entry`.

Errors: `409 conversation_reader_starting` (ask again shortly), `409 conversation_unavailable` (state cannot be served, runner did not answer in time, runner could not read), `409 conversation_record_unreadable`, `400 invalid_attempt` (attempt id is not 1–128 characters of `[A-Za-z0-9._-]`), `404`.

`ConversationEntryDto`:

```json
{ "entry_number": 17, "timestamp": "…", "source": "stdout",       // narsor | stdout | stderr
  "raw_base64": "eyJ0eXBlIjoi…",                                  // the raw line; diagnostic, not for display
  "elements": [ ChatElement ],                                     // what to show; ≥1 on a current server
  "subagent": { "id": "call-1", "name": "Explore the repo" } }    // omitted for the main agent
```

`ChatElement` is tagged with `kind`. Optional keys are omitted when absent.

```jsonc
{ "kind": "prompt", "text": "…", "resumed_session": "…", "purpose": "step", "author": "engine" }
//   purpose: step | reprompt | continuation | conductor        author: engine | engine_user | conductor | user
{ "kind": "user_message", "text": "…" }
{ "kind": "thinking", "text": "…", "redacted": true }            // redacted omitted when false
{ "kind": "agent_text", "text": "…" }
{ "kind": "agent_question", "question": "…", "options": ["a","b"], "source": "tool", "call_id": "…" }
//   source: output_contract | tool
{ "kind": "tool_call", "call_id": "…", "tool": "Read", "summary": "src/main.rs", "input": { … } }
{ "kind": "tool_result", "call_id": "…", "tool": "Read", "call_summary": "src/main.rs",
  "status": "ok", "content": "preview…", "truncated": true }     // status: ok | error
{ "kind": "session_started", "session_id": "…", "model": "…", "cli_version": "…", "cwd": "…", "permission_mode": "…" }
{ "kind": "run_result", "text": "…", "structured_output": { … }, "stop_reason": "…", "is_error": true,
  "cost_usd": 0.1, "usage": { … }, "subagent_stats": { … }, "turns": 12, "duration_ms": 34000 }
{ "kind": "technical", "category": "hook", "summary": "…", "data": { … } }
//   category: hook | compaction | system | stderr | process
{ "kind": "unclassified", "reason": "unknown_type" }
//   reason: unknown_type | invalid_json | too_large | harness_not_classified | unknown_block | not_recorded | unreadable
```

Text from agents and runners is not HTML-safe. Escape it.

### T13 `GET /control/v1/tasks/{task_id}/execution-events` (workflow events of the task)

| Query | Type | Default | Meaning |
|---|---|---|---|
| `after` | u64 | 0 | events with `sequence > after` |
| `limit` | u32 | 500 | server clamps to 1…2000. The TUI sends 500, at most 20 pages per read. |

Relayed like T12. Response 200, tagged with `status`:

```json
{ "status": "available", "events": [ Event ], "has_more": true, "source": "runner" }
{ "status": "unavailable", "reason": "reader_starting", "message": "…" }
```

When `has_more` is true, ask again with `after` = `sequence` of the last event. `unavailable` is a **200** answer (not started, paused, archived, reader starting, …). Errors: `503 runner_timeout | runner_read_failed` (retry), `404`.

`Event` (workflow event; `data` is **nested**, tagged with `type`):

```json
{ "id": "uuid", "workflow_execution_id": "…", "timestamp": "…", "sequence": 17,
  "data": { "type": "state_entered", "state_id": "work" } }
```

`data.type` values and their fields (`crates/narsor-protocol/src/event.rs:22`; `?` = nullable or defaulted):

| type | fields |
|---|---|
| `workflow_started` | `workflow_id`, `workflow_version`, `inputs` (any JSON) |
| `workflow_paused` | `reason` |
| `workflow_resumed` | — |
| `workflow_completed` | `result` (any JSON) |
| `workflow_failed` | `error` |
| `workflow_cancelled` | `reason` |
| `state_entered` | `state_id` |
| `state_exited` | `state_id`, `output_label` |
| `action_started` | `action_execution_id`, `state_id`, `action_type`, `decision_by?` |
| `action_completed` | `action_execution_id`, `output_label`, `reason?`, `additional_data` (object, omitted when empty), `warnings` (omitted when empty) |
| `action_failed` | `action_execution_id`, `error`, `error_type?` |
| `action_attempt_failed` | `action_execution_id`, `state_id`, `error`, `retry_in_ms` |
| `action_retrying` | `action_execution_id`, `retry_count`, `state_id?` |
| `action_output_rejected` | `action_execution_id`, `state_id`, `reason`, `answer_tail?` |
| `output_reprompted` | `action_execution_id`, `previous_attempt`, `state_id`, `by` (`engine`\|`conductor`) |
| `agent_session_started` | `session_id`, `harness`, `action_execution_id?`, `resumed_session?`, `model` (omitted when absent), `effort` (omitted when absent) |
| `agent_session_completed` | `session_id`, `action_execution_id?`, `harness_session_id?` |
| `agent_session_failed` | `session_id`, `error`, `action_execution_id?`, `harness_session_id?` |
| `permission_denied_ignored` | `session_id`, `action_execution_id?`, `tool_name`, `tool_use_id`, `summary`, `reason?`, `source` (`{id,name}`, omitted when absent) |
| `agent_message` | `session_id`, `content`, `role`, `action_execution_id?` |
| `decision_requested` | `provider` |
| `decision_made` | `provider`, `output_label`, `reasoning?` |
| `human_input_requested` | `instruction`, `reason?`, `required_values[]`, `answer_tail?`, `episode?` |
| `human_input_received` | `input` (any JSON), `answered_by?` (`operator`\|`conductor`), `close_reason?` (`answered`\|`transition`\|`recover`\|`cancel`\|`undo`), `episode?`, `output_label?`, `artifacts?`, `variables?`, `comment?` |
| `human_transition_selected` | `from_state`, `to_state` |
| `problem_raised` | `state_id`, `action_execution_id`, `kind` (`output`\|`failure`), `reason`, `episode?` |
| `conductor_started` | `invocation_id`, `trigger`, `purpose`, `episode?` |
| `conductor_finished` | `invocation_id`, `outcome` (`completed`\|`failed`\|`interrupted`), `error?` |
| `conductor_decision` | `decision_id`, `kind`, `state_id?`, `output_label?`, `episode?`, `confidence` (`high`\|`low`, default `low`), `rationale?` |
| `conductor_decision_undone` | `decision_id`, `state_id`, `warning?` |
| `external_event_received` | `event_type`, `source`, `payload` |
| `variable_set_by_workflow` | `variable`, `value`, `state_id`, `output_label`, `local` (bool) |
| `transition_taken` | `from_state`, `to_state`, `output_label` |
| `entry_limit_reached` | `state_id`, `entries`, `max_entries` |
| `entry_limit_decision` | `state_id`, `decision` |
| `entry_limit_counter_reset` | `state_id`, `previous_entries` |
| `artifact_created` | `artifact_id`, `artifact_type`, `location` |
| `error` | `message`, `context` |

- Conductor `trigger`: `output_rescue`, `step_failure`, `chat`, `annotation`, `title`, `decision_step`.
- Conductor `purpose`: `choose_output`, `choose_output_after_reprompt`, `answer_agent_question`, `diagnose_failure`, `reply_to_chat`, `summarize_gate`, `generate_title`, `decide_fork`.
- `conductor_decision.kind`: `choose_output`, `reprompt`, `escalate`, `retry`, `execute_proposal`, `answer_question`.

### T14 `GET /control/v1/tasks/{task_id}/conductor`

| Query | Type | Default | Meaning |
|---|---|---|---|
| `after` | u64 | 0 | chat entries with `sequence > after` |
| `limit` | u32 | 200 | server clamps to 1…1000. The TUI sends none. |

Relayed like T12. Response 200, tagged with `status`:

```json
{ "status": "available", "conductor": ConductorView, "source": "runner" }
{ "status": "unavailable", "reason": "paused", "message": "…" }
```

Errors: `503 runner_timeout | runner_read_failed`, `404`.

`ConductorView` (every key except `available` has a default):

```json
{
  "available": true,
  "unavailable_reason": null,
  "harness": "claude-code",
  "working": [ { "purpose": "reply_to_chat", "trigger": "chat", "episode": null, "since": "…", "running": true } ],
  "unanswered_chat": ["<chat entry id>"],
  "failure": FailureReport,                // or null
  "chat": [ ChatEntryDto ],
  "chat_has_more": false,                  // true → ask again with after = last chat sequence
  "proposals": [ ProposalView ],
  "decisions": [ DecisionView ]
}
```

```jsonc
// ChatEntryDto
{ "id": "…", "sequence": 12, "at": "…", "author": "conductor", "kind": "message", "text": "…",
  "in_reply_to": null, "payload": null }
// author: operator | conductor      kind: message | error | decision | proposal      payload: any JSON or null

// ProposalView
{ "proposal_id": "…", "text": "…", "at": "…", "in_reply_to": null, "state": "pending",
  "record": { "episode": null, "state_id": "work",
              "proposal": { "kind": "retry", "summary": null, "label": null, "values": {}, "text": "note" },
              "confidence": "high", "rationale": "…" } }
// state: pending | executed | rejected | stale
// proposal.kind: answer_input | message_agent | retry | transition | abort

// DecisionView
{ "decision_id": "…", "kind": "choose_output", "text": "…", "at": "…", "state_id": "work",
  "confidence": "low", "rationale": null, "undone": false, "undoable": true }
// kind is a free string here

// FailureReport — outcome is tagged with "kind"
{ "key": "…", "episode": "…", "state_id": "work", "reason": "…", "concluded_at": "…",
  "outcome": { "kind": "diagnosed", "diagnosis": "…", "proposals": [ { "kind": "retry", … } ] } }
//          { "kind": "retried", "note": "…" }
//          { "kind": "unanswered", "error": "…" }
// The failure is a problem unless outcome.kind == "retried".
```

To act: T7 with `confirm_proposal` / `reject_proposal` (`proposal_id`), `undo_decision` (`decision_id`, only when `undoable`), `conductor_chat` (`text`), `restart_step` (`failure_key` = `failure.key`).

---

## 8. Materials (CLI only today)

### M1 `GET …/materials` → `{ "materials": [ MaterialSummary ] }`

### M2 `POST …/materials`

Header `Content-Type: application/vnd.narsor.material-bundle` (parameters after `;` are permitted). Else `400 invalid_content_type`.

Body = binary bundle (`crates/narsor-engine/src/materials/bundle.rs`):

```
"NMB1"            4 bytes
manifest length   4 bytes, big-endian u32
manifest          JSON UploadManifest (max 1 MiB)
blob, blob, …     bytes of each distinct sha256, in order of first appearance in manifest.files,
                  each exactly `size` bytes; nothing after
```

`UploadManifest` (**deny_unknown_fields**, also for each file):

```json
{ "material_id": "m-1", "label": "specs", "ttl_secs": 86400,
  "files": [ { "path": "docs/a.md", "size": 120, "sha256": "<64 hex chars, lowercase>" } ] }
```

`material_id`, `label`, `ttl_secs` are optional. Send `material_id` so a retry is idempotent. Default limits: file 16 MiB, material 64 MiB, 1000 files, 32 materials and 256 MiB per task, TTL default 7 days, max 30 days; path max 1024 bytes, depth max 32.

Response **201** `{ "material": MaterialSummary, "replayed": false }`; **200** with `replayed: true` for a repeat.
Errors: `400 invalid_material_manifest | invalid_material_path | material_too_large | material_digest_mismatch | material_incomplete | material_refused`, `409 material_id_reused | material_quota_exceeded`, `404`.

### M3 `GET …/materials/{material_id}` → `MaterialView` = the summary keys **flattened** plus `files`

```json
{ "material_id": "m-1", "task_id": "task-a", "origin": "owner", "label": "specs",
  "created_at": "…", "expires_at": "…", "file_count": 1, "total_bytes": 120,
  "manifest_digest": "…", "delivered_at": null,
  "files": [ { "path": "docs/a.md", "size": 120, "sha256": "…" } ] }
```

`origin`: `owner` | `runner`. `MaterialSummary` = the same object without `files`.

### M4 `DELETE …/materials/{material_id}` → 200 `MaterialSummary` (the removed one).

### M5 `GET …/materials/{material_id}/blobs/{sha256}` → 200 `application/octet-stream`, the raw bytes. The Rust client checks length and SHA-256 against the manifest. `404` when deleted or expired.

---

## 9. Settings and capabilities

### C1 `GET /control/v1/capabilities`

```json
{ "connection_name": "local", "workflow_paths": true,
  "workflow_paths_reason": "…",        // omitted when workflow_paths is true
  "legacy_executions": false, "local_repository_paths": true }
```

`workflow_paths` = W3 and `POST /tasks` with a file path can work (trusted local daemon only). `legacy_executions` = H1 has rows. `local_repository_paths` = R2 accepts a path on the server's disk.

### C2 / C3 `/control/v1/default-backend`

GET → `DefaultBackendView`; PUT body `{ "backend_id": "k8s" }` or `{ "backend_id": null }` (clear) → the same view.

```json
{ "configured": "local", "override": null, "effective": "local", "revision": 0, "updated_at": null }
```

Note the key is `override`. PUT errors: `400 invalid_identifier`, `400 <launch error code>` for a backend that is not registered or not usable now (the code comes from `LaunchError::code()`, `service/tasks/create.rs:166`; I did not list its values).

### C4 / C5 `/control/v1/concurrency-limit`

GET → `ConcurrencyLimitView`; PUT body `{ "limit": 3 }` (1…1024, required) → the same view. `400 invalid_concurrency_limit`. The change lasts until the control plane restarts.

```json
{ "limit": 3, "slots_held": 2, "overshoot": 0, "waiting_to_stop": 0 }      // limit: null = no limit
```

### C6 `GET /control/v1/usage`

```json
{ "harnesses": [
  { "harness": "claude-code", "observed_at": "…", "received_at": "…",
    "limits": [ { "window": "5h", "utilization": 0.42, "resets_at": "…", "status": "allowed" },
                { "window": "7d", "utilization": 0.9,  "resets_at": "…", "status": "allowed_warning" } ] } ] }
```

- `harnesses` is empty when no fresh report exists (the server keeps a report 15 minutes after it received it). Order: fixed harness order; `limits`: shortest window first.
- `window`: count 1–9999 then `m`, `h` or `d`. `utilization`: 0…1 (fraction, not percent). `status`: `allowed` | `allowed_warning` | `rejected`; the key is omitted when the harness did not say.
- This route cannot fail with a protocol error (only 401/403).

### C7 `/settings/agent-harness` (note: not under `/control/v1`)

- GET → `HarnessSelectionSnapshot`.
- PUT body `{ "harness": "codex" }` → snapshot after the change. `400 unsupported_harness`.
- DELETE (no body) → snapshot with the override removed.

```json
{ "supported": ["claude-code", "codex", "opencode", "github-copilot"],
  "configured": "claude-code", "override": null, "effective": "claude-code", "revision": 3 }
```

Known harness ids and names: `claude-code` (claude / Claude Code), `codex` (codex / Codex), `opencode` (opencode / OpenCode), `github-copilot` (copilot / GitHub Copilot).

---

## 10. Workflows and repositories

### W1 `GET /control/v1/workflows` → `{ "workflows": [ WorkflowView ] }`

```json
{
  "name": "feature",
  "source": "built_in",               // built_in | catalog | user
  "description": "…",                 // omitted when absent
  "version": 3,                       // omitted when the definition does not load
  "state_count": 7,
  "initial_state": "plan",            // omitted when absent
  "inputs": [ { "name": "ticket", "type": "string", "required": true, "description": "…", "default": "x" } ],
  "workspaces": [ { "name": "main", "description": "…", "base_branch": "develop" } ],
  "valid": true,
  "violations": ["…"],                // why it cannot be chosen (stays listed)
  "overrides": false,
  "origin_label": "built-in",
  "shadow_labels": ["built-in"],
  "has_builtin": true                 // omitted by an old server; do not infer absence
}
```

`inputs[].type`: `string` | `text` | `integer` | `boolean`. `description` and `default` (any JSON) are omitted when absent. `workspaces` = the workspaces whose repository the owner must choose (T2 `launch.workspaces`).

### W2 `POST /control/v1/workflows/source`

Body `{ "name": "feature", "builtin": false }` (`builtin` default false). Response `{ "name": "…", "origin_label": "…", "content": "<YAML text>" }`.
Errors: `404` unknown name; `400 workflow_source_unavailable` on a non-local server when `builtin` is false.

### W3 `POST /control/v1/workflows/validate`

Body `{ "path": "/abs/path/on/the/server/flow.yaml" }`. The file is on the **server's** disk. Only a trusted local daemon reads it.

Response 200 `ValidateWorkflowResponse` (a workflow with violations is a 200 with `valid:false`):

```json
{ "valid": true, "name": "feature", "version": 3, "state_count": 7, "initial_state": "plan",
  "inputs": [ … ], "workspaces": [ … ], "violations": [] }
```

`name`, `version`, `initial_state` are omitted when not valid. Errors: `400 workflow_paths_unavailable | workflow_not_yaml | workflow_not_found | workflow_unreadable | workflow_not_a_file | workflow_too_large`.

### W4 `POST /tasks` (create a task from a workflow reference or file path)

Body `StartTaskRequest`:

```json
{ "task_id": "task-3f2a…",                 // required
  "workflow": "/abs/path/flow.yaml",       // unresolved reference: a name or a path on the server
  "inputs": { "ticket": "ABC-1" },         // object, default {}
  "workspaces": { "main": { "repository": "repo-id", "branch": "develop" } },   // optional
  "ticket": "ABC-1", "title": "…" }        // optional
```

Response **202** `{ "task": TaskSummary }`. A retry with the same `task_id` and the identical body replays the answer.
Errors: `400 invalid_identifier | invalid_workflow | invalid_task_label | <launch error code>`, `409 task_conflict`, bare `500` when the server has no local scheduler.

### R1 `GET /control/v1/repositories`

```json
{ "repositories": [ { "id": "narsor", "name": "narsor", "provider": "gitlab", "remote": "https://host/x.git",
                      "default_branch": "main", "description": "…" } ],
  "unreadable": ["bad.yaml: reason"] }
```

`default_branch`, `description` are omitted when absent. Credentials in `remote` are removed by the server.

### R2 `POST /control/v1/repositories`

Body `CreateRepositoryRequest` (**deny_unknown_fields**): `{ "id": "narsor", "remote": "git@host:group/x.git", "name": null, "provider": null, "default_branch": null, "description": null }` (only `id` and `remote` are required).
Response **201** `RepositoryView`. Errors: `400 invalid_repository` (control characters; or, on a non-local server, a remote that is not `https://`, `ssh://`, `git://` or `user@host:path`), `409 repository_exists`.

### R3 `DELETE /control/v1/repositories/{id}` → 200 `RepositoryView` (the removed one). `404` unknown id.

### R4 `GET /control/v1/repositories/{id}/branches` → `{ "branches": ["main", "develop"] }`

The server runs `git ls-remote` (up to 20 s; the Rust client waits 25 s). Errors: `404` unknown id, `400 branches_refused`, `503 branches_unavailable`. Never an empty list for a failure.

---

## 11. History (executions from before the task scheduler)

All H routes answer `404` on a server that is not the trusted local daemon, and for an unknown id. H1 answers an empty list instead.

### H1 `GET /control/v1/executions` → `{ "executions": [ LegacyExecutionView ] }` (newest first)

```json
{ "id": "…", "workflow_id": "feature", "workflow_version": 3, "status": "failed",
  "current_state": "work", "started_at": "…", "completed_at": null,
  "problem": { "kind": "failure", "reason": "…", "episode": "…" },     // or null; kind: output | failure
  "recoverable": true,
  "not_recoverable_reason": "…" }      // omitted when recoverable
```

### H2 `GET /control/v1/executions/{execution_id}` → `LegacyExecutionView`

### H3 `GET …/events?after=N` → `Event[]` (section 7, T13 `Event`), `sequence > after`, no page limit. `after` default 0.

### H4 `GET …/timeline` → `Timeline` (bare; not wrapped in `TimelineResponse`).

### H5 `GET …/attempts/{attempt_id}/conversation?from=N&byte_budget=B` → `ConversationPage` (section 7, T12)

Defaults here: `from` = **1**, `byte_budget` = 1048576 (no clamp). The TUI sends only `from`. Read from the daemon's disk (no relay, no 409). An unsafe attempt id answers `{"status":"no_record"}`.

### H6 `GET …/conductor` → `ConductorStateResponse`

```json
{ "pending": { "invocation_id": "…",
               "request": { "request_id": "…", "trigger": "chat", "purpose": "reply_to_chat",
                            "episode": null, "payload": null, "queued_at": "…" },
               "started_at": "…" },               // or null
  "queue": [ { "request_id": "…", "trigger": "…", "purpose": "…", "episode": null, "payload": {…}, "queued_at": "…" } ],
  "unanswered_chat": ["<chat entry id>"],
  "invocation_attempts": { "<episode>": 1 },
  "used_retries": ["<episode>"],
  "unapplied_decisions": [ { … } ],                // opaque objects for the client
  "failure": FailureReport }                       // or null (section 7)
```

This is a different shape from T14 `ConductorView`. An execution without conductor data answers the empty state.

### H7 `GET …/conductor/chat?after=N` → `{ "entries": [ ChatEntryDto ] }` (`sequence > after`, no page limit).

### H8 `POST …/recover`

Body `RecoveryRequest`: `{ "comment": "text or null" }` (optional `episode` string; the TUI does not send it). Response **202** `{ "execution_id": "…" }`.
Errors: `409 not_recoverable` with the reason in `message`; `404`.

---

## 12. Legacy `/executions/...` routes (the TUI does not use them)

They work only for executions in the daemon's own store. For a task's (runner-owned) execution they answer a bare `409`. Listed for completeness:

| Method | Path | Body | Success |
|---|---|---|---|
| POST | `/executions/{id}/conductor/chat` | `{ "text": "…" }` | 202 `ChatEntryDto`; `400 empty_message` |
| POST | `/executions/{id}/conductor/proposals/{proposal_id}/confirm` | none | 200 `ProposalOutcome` |
| POST | `/executions/{id}/conductor/proposals/{proposal_id}/reject` | none | 200 `ProposalOutcome` |
| POST | `/executions/{id}/conductor/decisions/{decision_id}/undo` | none | 200 `UndoOutcome` |

```json
// ProposalOutcome
{ "proposal_id": "…", "status": "executed", "decision_id": "…", "message": "…", "repeated": false }   // status: executed | rejected
// UndoOutcome
{ "decision_id": "…", "state_id": "work", "episode": "…", "warning": null, "message": "…" }
```

Errors: `409 stale_proposal | proposal_settled | execution_busy | already_undone | decision_superseded | execution_cancelled`, `400 invalid_proposal | not_undoable`, `404`.

Other legacy routes (`GET /executions`, `/executions/{id}/events|timeline|stream|resume|recover|interrupt|input|permission|transition|operator-note`, `/events/stream`) are used by the CLI only. They are not described here. See `crates/narsor-server/src/api/routes.rs:335-390`.

---

## 13. Pagination summary

| Read | Cursor param | Next cursor | Page size param | "More" signal |
|---|---|---|---|---|
| S1 event stream | `cursor` / `Last-Event-ID` (composite string) | `envelope.cursor` | — | stream |
| T9 task events | `after` (u64, exclusive) | last `sequence` | none (all) | — |
| T12 conversation page | `from` (u64, **inclusive**) | `next_entry` | `byte_budget` | not `closed`, not `truncated_tail`, not empty, `next_entry > from` |
| T13 execution events | `after` (exclusive) | last `event.sequence` | `limit` | `has_more` |
| T14 conductor chat | `after` (exclusive) | last `chat[].sequence` | `limit` | `conductor.chat_has_more` |
| H3 / H7 | `after` (exclusive) | last `sequence` | none (all) | — |
| H5 | `from` (inclusive, default 1) | `next_entry` | `byte_budget` | as T12 |
| T1 task list | none | — | none | — |

---

## 14. Things that are surprising or easy to get wrong

1. **Success codes differ.** T2, R2 → 201. T6, T7, T8, W4, H8 → 202. M2 → 201, or 200 for a replay. Treat 200/201/202 the same.
2. **401, 403, 404 and 500 have no body.** A 400 can have a JSON body (`{code,message}`), an empty body (legacy), or a **plain-text** body (axum rejection of the query or JSON). 409 can be JSON or empty. Do not call `response.json()` without a guard.
3. **404 also means "not yours"** and "this server does not do that" (history routes on a non-local server). `claim` in trusted mode is 404.
4. **`unavailable` is a 200** for T11, T13, T14 (`status:"unavailable"`). But T12 uses **409** for the same conditions (`conversation_reader_starting`, `conversation_unavailable`), and T13/T14 use **503** for a runner that did not answer.
5. **Relayed reads are slow** (T12, T13, T14): the server waits up to 20 s for the runner. Use a long client timeout or cancel and ask again. A stopped task starts a read-only runner: expect `reader_starting`, then ask again.
6. **Operator command body is flattened** in T7 (`kind` at the top level beside `idempotency_key`), but nested as `command: {kind…}` in `OperatorCommandView`, in task events and in `TaskActionSend`.
7. **`override`** is the wire name of `Transition.forced`, of `DefaultBackendView.override_backend` and of `HarnessSelectionSnapshot.override_harness`.
8. **Nested tags with the same name**: `TaskSummary.queue.reason.reason` (`QueueView.reason` is an object tagged with `reason`); task event `{"type":"queue_changed","reason":{"reason":"admitted"}}`.
9. **Three different tag names**: `type` (task event kind, workflow `Event.data`, `TaskActionSend`), `kind` (operator command, `ChatElement`, observation, `FailureOutcome`), `status` (conversation/events/conductor responses), `state` (`Position.conversation`), `link`+`value` (`EventLink`), `to`+`task_id` (move target), `reason` (queue reason).
10. **`PanelPlan` and `PanelFieldKind` are externally tagged**: `"answer"` (bare string) versus `{"output":{"label":"x"}}`. Check `typeof plan === "string"`.
11. **Two "events"**: T9 and the stream carry control-plane `TaskEvent` (field `kind`). T13 and H3 carry workflow `Event` (field `data`). Two "conversation responses": T11 (`available`/`unavailable` summary) and T12/H5 (`recorded`/`no_record` page). Two conductor shapes: T14 `ConductorView` and H6 `ConductorStateResponse`.
12. **Conversation `from` is inclusive**; all `after` parameters are exclusive. T12 default `from` is 0, H5 default is 1.
13. **`Timeline.links` keys are strings** (sequence numbers as JSON object keys).
14. **Always-present nullable keys** in `TaskSummary`: `archive`, `observation`, `runner`, `controller` are written as `null`. Other optional keys (`ticket`, `title`, `state_since`, `steps`, `actions`, `panel`, `manual_transitions`) are **omitted**. Use `?? null` everywhere.
15. **`state` versus `runner_state`**: show `state`. `runner_state` describes resources. `stopped` was `paused` in old records; `runner_state` was `lifecycle`.
16. **The stream cursor is composite** (`task:seq,task:seq`), not a number. No cursor = full replay of all history. Take the first cursor from T1 (`cursor`). `""` is valid.
17. **EventSource cannot be used** (no Authorization header). The stream ends when the token expires; reconnect from the last cursor with a fresh token.
18. **CORS is off by default.** The server sends CORS headers only for origins in `cors_allowed_origins`. Allowed request headers are only `authorization`, `content-type`, `last-event-id`. A custom header will fail the preflight.
19. **A doc comment says `POST …/comments`** (`controller/task/actions.rs:283`), but the route is **`/comment`** (singular).
20. **`TaskActionOffer.send` placeholders**: only a string that is exactly `{{name}}` is replaced; a blank field gives `null`, not `""`.
21. **Field names in panel definitions are Polish literals** (`Wiadomość dla agenta`, `Komentarz dla agenta`, `Wzorzec polecenia`, `komentarz`, `variables (klucz=wartość)`), and the answer plan needs a field named `answer` or `option`. They are protocol constants (`crates/narsor-protocol/src/panel/mod.rs:38-41`, `panel/action.rs`, `panel/permission.rs`).
22. **deny_unknown_fields** on `launch`, each `workspaces` value, `CreateRepositoryRequest`, `UploadManifest` and its files: an extra key is a 422 with a text body.
23. **`archive` cannot be undone** and wins over every other state. `expected_generation` protects against acting on stale state; send it.
24. **`utilization` is a fraction 0…1**, and `/control/v1/usage` is empty (not an error) when no runner reported in the last 15 minutes.
25. **`/settings/agent-harness` and `POST /tasks` are not under `/control/v1`.**
26. **`W3` and `W4` take a path on the server's disk**, not an upload. Check `capabilities.workflow_paths` first.
27. **Text is cleaned of control characters by the server for a terminal, not for HTML.** Escape all agent, runner and workflow text in the browser. Accept `pr_url` as a link only when it is an absolute `http(s)` URL.

## 15. Open points (not verified)

- Exact bytes of the SSE keep-alive comment (`: keep-alive` or `:keep-alive`): `crates/narsor-server/src/controller_protocol/http.rs:682`.
- The values of the launch refusal codes (`LaunchError::code()`), used as `400` codes in T2, W4 and `PUT /control/v1/default-backend`: `crates/narsor-engine/src/controller_protocol/service/tasks/create.rs:166`, `crates/narsor-engine/src/environment/`.
- Rules that decide which `actions` and `panel.forms` the server offers in each state: `crates/narsor-engine/src/controller_protocol/service/tasks/actions.rs` and `summary.rs`. A client only draws what it gets.
- The error-code lists in section 1.4 come from a grep and are not complete.
- No request was run against a live server. JSON examples are derived from the serde attributes.
