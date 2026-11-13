# Part A — application shell, header, task list, details header, footer, refresh model

Paths are relative to `/home/artur/Projekty/narsor`. All rules below come from the code. Section A.11 lists the places where `docs/user/tui.md` disagrees with the code.

Abbreviations: `mod.rs` = `src/tui/task_app/mod.rs`, `list.rs` = `src/tui/task_app/list.rs`, `detail.rs` = `src/tui/task_app/detail.rs`, `layout.rs` = `src/tui/task_app/layout.rs`.

---

## A.1 Application structure

### Event loop (`src/tui/mod.rs:232-302`)

- One application object: `TaskApp` (`mod.rs:424-499`). One event feed: `EventFeed` (`mod.rs:1678-1730`).
- Loop order (`src/tui/mod.rs:258-296`): draw, then `select!` over (a) key or paste, (b) next SSE item `feed.next()` → `app.on_stream`, (c) outcome of a background read → `app.on_outcome`, (d) sleep → `app.advance_animation()`.
- After each iteration: `feed.ensure_open`, `start_refresh_if_due`, `start_usage_if_due`, `retry_outgoing_if_due`, `start_detail_reads`, `feed.ensure_open`.
- Constants: `TICK = 250 ms` (redraw at least this often, `src/tui/mod.rs:89`), `KEY_POLL_INTERVAL = 120 ms` (`:83`).
- The first read starts after the first draw (`:263-269`), so the screen opens with `łączę z …` when the server is not reachable.
- A key becomes a `Request` (`TaskApp::on_key`, `mod.rs:812`), then `TaskApp::dispatch` executes it (`mod.rs:1152-1173`). A write is awaited. A read is a background job.

### Enums that define the UI state

| Enum | Values | Where |
|---|---|---|
| `Focus` | `List`, `Details` | `mod.rs:145-150` |
| `StepFocus` (only on *Przebieg*) | `Axis` (default), `Conversation` | `mod.rs:155-163` |
| `DetailTab` | `Progress`, `Conversation`, `Events`, `Conductor`, `Info` | `mod.rs:166-178` |
| `View` / `ViewKind` | `Tasks`, `NewTask`, `History`, `Repositories`, `Opening(OpeningKind)` | `mod.rs:342-369` |
| `OpeningKind` | `NewTask`, `History`, `Repositories`, `Harness` | `background.rs:60-69` |
| `Screen` (geometry) | `Split`, `Full`, `TooSmall` | `layout.rs:65-72` |
| `LinkState` | `Connecting`, `Live`, `Retrying{next_attempt}`, `Disconnected` | `src/tui/connection.rs:31-43` |
| `Request` | `None`, `OpenNewTask`, `OpenHistory`, `OpenRepositories`, `SendTask(id)`, `ValidatePath(path)`, `Recover(id)`, `AddRepository`, `RemoveRepository(id)`, `Move(MoveRequest)`, `Answer`, `Act`, `OpenLink`, `OpenHarness`, `OpenLimit`, `ReadBranches(Vec<String>)`, `SetLimit{limit}`, `SetHarness(Option<String>)` | `mod.rs:373-415` |

Other state flags of `TaskApp` (`mod.rs:424-499`): `help_open`, `overlay: Option<Overlay>` (action panel, link list, comment form, `y`/`n` window), `panel_focus` (keys in the answer panel), `quit_confirm`, `new_task_memory` (the form left with `Esc`), `outbox` (tasks being created), `unsent_messages` (per task), `action_keys` (idempotency keys per decision), `harness_pending`.

### Tab order and default tab (code)

- `DetailTab::ALL` = `[Conversation, Progress, Events, Conductor, Info]` (`mod.rs:182-188`). The visible order is **Rozmowa, Przebieg, Zdarzenia, Dyrygent, Info**.
- The default tab is `DetailTab::Conversation` (*Rozmowa*) (`mod.rs:236`).
- The tab is global (one value for all tasks, `DetailState.tab`), not per task.
- `[` / `]` call `shift_tab(∓1)`; the order wraps round (`mod.rs:202-206`, `:994-997`). A tab change puts the *Przebieg* focus back on the axis.
- Tab titles verbatim (`mod.rs:191-199`): `Przebieg`, `Rozmowa`, `Zdarzenia`, `Dyrygent`, `Info`.

### Key routing precedence (`mod.rs:812-950`)

1. List filter entry (`/` active, view `Tasks`, focus `List`): all keys go to the filter.
2. Quit confirmation: `q` (no modifier) quits, `Esc` cancels, other keys do nothing.
3. Help open: any key closes it. Only a key that maps to `Quit` continues.
4. Overlay open: the overlay takes the key.
5. View: `Opening` (only `Esc` and `q`), `NewTask` / `Repositories` with focus `Details`, `History` (always).
6. Answer panel focused → panel keys.
7. Conductor text box active → `Tab`/`BackTab` go to the panel, other keys are text.
8. Otherwise the key map of the context (`key_context`, `mod.rs:692-733`).

Context layers over the details (`detail_keys`, `mod.rs:212-221`): *Przebieg* + `StepFocus::Conversation` → `StepConversation`; *Przebieg* + axis → `Axis` (`HistoryAxis` in history); *Rozmowa* → `AgentConversation`; other tabs → `Reading`.

A view-opening key (`n`, `H`, `R`, `h`, `L`) pressed while the view is not `Tasks` gives the warning `zakończ bieżący formularz przed zmianą widoku` (`mod.rs:877-888`).

### Layouts (`layout.rs:51-141`)

- `MIN_WIDTH = 60`, `MIN_HEIGHT = 20`, `SPLIT_MIN_WIDTH = 100`, `LIST_WIDTH = 28` (list column with its frame).
- Width ≥ 100: list (28 columns) and details side by side.
- Width < 100: one panel at a time, the one that has the focus (`Enter` → details, `Esc` → list).
- Below 60×20: message `Terminal za mały — powiększ okno` and `min. 60×20, jest {w}×{h}` (`layout.rs:257-279`).
- The details area shows: the selected task (`View::Tasks`), or the new-task form, or the history details, or the repositories. The history also replaces the list (`layout.rs:179-190`, `:239-254`).
- A view that waits for its read shows a panel with its title and `⠋ wczytuję…` (`layout.rs:230-238`). Titles: ` Nowe zadanie `, ` Historia `, ` Repozytoria `, ` Domyślny harness ` (`background.rs:73-80`).

### Opening a view (`mod.rs:1188-1248`, `background.rs:104-145`)

| Key → Request | Reads (client methods) |
|---|---|
| `n` → `OpenNewTask` | `control_capabilities()` (only when not yet read), `control_workflows()`, `control_repositories()` — in parallel |
| `H` → `OpenHistory` | `control_tasks_archived(true)`, `control_history()` — in parallel |
| `R` → `OpenRepositories` | `control_capabilities()` (only when not yet read), `control_repositories()` |
| `h` → `OpenHarness` | `harness_selection()` |
| `L` → `OpenLimit` | `start_limit_read()` (see `limit.rs`, other part) |

- A ticket counter (`open_ticket`) discards the answer of a view that the operator left with `Esc` (`mod.rs:1214-1225`).
- `H` or `R` pressed again while that view is open reloads it (`mod.rs:1189-1198`).
- `enter_view` remembers the previous focus; `leave_view` restores it (`mod.rs:1483-1500`).

### Quit (`mod.rs:965-967`, `:818-834`, `actions.rs:1571-1596`)

- `q` always opens a confirmation (code: always, not only when a draft exists). Second `q` quits. `Esc` cancels.
- Window title `Wyjść z TUI?`; text `Naciśnij ponownie q, aby wyjść.`; with drafts also `Niewysłany szkic zostanie utracony.`; hints `q wyjdź`, `Esc anuluj`.

---

## A.2 Header row (first screen row)

Source: `layout.rs:284-436`.

Left part (`header_left`, `layout.rs:351-368`):

```
 narsor · {connection name} {icon} {state text}[ · stan sprzed {age}] 
```

Right part (`header_right`, `layout.rs:405-436`):

```
 harness {name}[ (konfiguracja)|…] · sloty {used}[/{limit}] · ? {questions} · ! {problems} 
```

### Connection name

- Initial name: the `--server` value, or `local` (`src/cli/commands/tui.rs:22`).
- After `control_capabilities()` returns: `capabilities.connection_name` (`mod.rs:601-605`).

### Connection states (`src/tui/connection.rs:155-178`)

| State | Icon | Text | Role (colour) |
|---|---|---|---|
| `Connecting` | `◌` | `łączę z {name}` | Attention (yellow, bold) |
| `Live` | `●` | `live` | Live (green) |
| `Retrying` | `◌` | `ponawiam ({n}s)` — whole seconds, rounded up, never `0s` while the attempt is ahead (`:201-204`) | Attention |
| `Disconnected` | `✖` | `brak połączenia` | Problem (red) |

State transitions:
- `on_data(now)` → `Live`. Called by each successful list read (`mod.rs:1459`) and each SSE event (`mod.rs:1595`).
- A read failure or a stream failure → `mark_failure` (`mod.rs:1567-1573`):
  - error is not a transport error → `Disconnected`;
  - transport error and data arrived at least once → `Retrying` with the delay;
  - transport error and no data ever → the state stays `Connecting`.
- Transport error = `ClientError::Unavailable | Server | Degraded` (`mod.rs:1646-1651`).
- A write that fails with a transport error also counts as a failed read (`note_transport`, `mod.rs:1576-1582`).
- SSE stream ended (`None`) or a non-transport stream error → `stream_ended = true`, `Disconnected`. For the error case also the notification `strumień zdarzeń zakończony: {error}` (`mod.rs:1619-1630`).

### Stale data

- `is_stale()` = not live and data arrived at least once (`connection.rs:150-152`). Then all list rows and the details are drawn in the Done role (grey).
- Age label: `stan sprzed {short_age}`; shown only when not live (`connection.rs:136-146`).
- `short_age(seconds)` (`src/cli/task_text.rs:148-156`): `0..59` → `{s}s`; `60..3599` → `{m}m`; `3600..86399` → `{h}h`; else `{d}d` (integer division, one unit only).

### Mutation gate (offline block)

`Connection::mutation_gate` (`connection.rs:182-196`). Each mutation calls `TaskApp::mutation_allowed()` first (`mod.rs:764-778`). When the connection is not live, the action does not run and a Warning notification shows:

- `{state} ({age}): akcja zmieniająca stan jest zablokowana` when an age exists;
- `{state}: akcja zmieniająca stan jest zablokowana` otherwise.

`{state}` = `łączę z backendem` (Connecting) | `ponawiam połączenie` (Retrying) | `brak połączenia` (Disconnected). `{age}` = `stan sprzed 40s`.

### Harness text (`layout.rs:371-402`, `harness.rs:131-168`)

| Condition | Text |
|---|---|
| No `harness_selection` read yet | `harness ?` |
| A change was sent, no answer yet (`harness_pending`) | `harness {new}…` |
| `snapshot.override_harness` is `None` | `harness {snapshot.effective} (konfiguracja)` (short form ` (konf.)`) |
| `snapshot.override_harness` is set | `harness {snapshot.effective}` |

### Slots (`list.rs:910-933`)

- Backend = the backend of the selected task; if no task is selected, the backend of the first task.
- If a task of that backend has `queue.reason == WaitingForCapacity{running, limit}` → `sloty {running}/{limit}`.
- Otherwise `sloty {count of tasks of that backend with holds_slot == true}` (no limit).
- No tasks → no slots text.

### Counters `? n` and `! n` (`list.rs:896-907`)

- For each task in `tasks` (all tasks, also those hidden by `z` or by the `/` filter; pending rows are not counted): compute `state_icon(task)`.
- `StateIcon::NeedsReply` → `questions += 1`. `StateIcon::Problem` → `problems += 1`.
- A task counts once. A problem wins over a question.
- Role: Done (grey) when the count is 0; else Attention for `?` and Problem for `!` (`layout.rs:423-433`).

### Shortening order when the row is too narrow (`layout.rs:300-348`)

1. remove the slots; 2. remove the age label; 3. `(konfiguracja)` → `(konf.)`; 4. remove the connection name; 5. cut the harness name (min 1 cell, with `…`). The counters always stay.

---

## A.3 Task list

### Order and content

- `GET /control/v1/tasks?archived=false` gives the tasks in queue order (grouped by backend, by `rank`). **The client never sorts** (`list.rs:1-21`).
- Only a `Moved` event reorders rows (it carries the full new order of the backend).
- Pending rows (tasks created here, not yet confirmed by the server) are shown after the server rows (`list.rs:668-681`).
- Panel title (`list.rs:1365-1380`): ` Zadania`; with hidden tasks ` Zadania · ✓ ukryte {n}`; with a filter ` / {filter}` appended (and `|` as the cursor while the filter is edited); with a running refresh and a non-empty list, the spinner glyph appended.
- Empty list (`layout.rs:438-457`): `⠋ wczytuję zadania…` while a refresh runs; `brak zadań` when live; `brak danych` when not live.

### Row layout — 3 lines (`list.rs:1615-1654`, `:795-805`)

```
{icon}  {ticket | id:{first 11 chars of task_id}}
   {title | task_id}
   {label}[ · {activity age}]
```

| Line | Content | Source fields |
|---|---|---|
| 1 | icon glyph, 2 spaces, identifier | `state_icon(task)`; `task.ticket` if not blank, else `id:` + first `TASK_ID_CHARS = 11` characters of `task.task_id` (`detail.rs:503-508`, `list.rs:333-335`) |
| 2 | title | `task.title` if not blank, else the full `task.task_id` (`title_of`, `list.rs:312-314`) |
| 3 | label and age | `label_of` + `activity_since` (below) |

- All server text goes through `one_line` (control characters removed, one line).
- A too-long text is cut with `…` (`fit_cells`, `detail.rs:1775-1794`).
- Loud rows: if the icon is `?` or `!` (`needs_operator`), the icon and the title are bold.
- Stale connection: all roles become Done (grey).

**Label (line 3, before the time)** — `label_of` (`list.rs:349-355`):
- `state == Queued` → `#{n}`, where `n` is the 1-based place among tasks of the same `backend_id` with `runner_state == Queued`, counted in list order (`queue_positions`, `list.rs:767-778`). Empty when the task is not counted.
- `state == Running` → current step: `task.runner.execution_cursor.current_state` (not blank), with the step icon before it (`{icon} {name}`), see step icons below.
- All other states → empty.
- Move mode on this row → `↕ {slot+1}/{rows of the backend that are shown}` (`list.rs:1397-1401`).
- Pending rows → `wysyłanie…` or `nie wysłano`.
- Label role: `nie wysłano` → Problem; label that starts with `#` → Calm; other → Done (`list.rs:1603-1609`).

**Activity start (`activity_since`, `list.rs:321-331`)**:
1. `runner_state == Provisioning` → `task.state_since`.
2. Else by `task.state`:
   - `Queued` → `task.queue.since`;
   - `HumanNeeded` → the minimum `since` of `task.attention` items;
   - `Finished | Error | Archived` → none (no time);
   - other (`Running`, `Paused`, `Unrecognized`) → `task.state_since`.
3. No value → no time.

**Third-line composition (`third_line`, `list.rs:1585-1601`)**: no age → the label only; no label → the age only; both → `{label} · {age}`. On a narrow list the label is cut first; if no room is left for the label, only the age stays.

**Elapsed-time format on the list — `activity_age(seconds)` (`src/cli/task_text.rs:159-175`)**, `seconds = now − since`, negative → 0:

| Range | Text |
|---|---|
| 0–59 s | `<1m` |
| 60–3599 s | `{minutes}m` |
| 3600–86399 s | `{h}h` when minutes = 0, else `{h}h {m}m` |
| ≥ 86400 s | `{days}d` |

The time is computed again at each redraw from the client clock (`Utc::now()`).

**Step icons** (`step_icon`, `detail.rs:1731-1742`; used on the list label, the status row and the timeline). Input: `action_type` and `decision_by`.

| `action_type` | `decision_by` | Icon |
|---|---|---|
| `agent` | any | `✨` |
| `script` | any | `💻` |
| `profile` | any | `⚖️` |
| `wait` | any | `✋` |
| `decision` | `conductor` | `⚡🔀` |
| `decision` | `human` | `🧑🔀` |
| `decision` | other / none | `🔀` |
| other / none | — | no icon |

For the list and the status row the inputs are `runner.execution_cursor.current_action_type` and `.current_decision_by`. For the timeline they are `position.action_type` and `position.decision_by`. Text: `{icon} {label}`.

### TaskSummary fields used by Part A files

`crates/narsor-protocol/src/controller/task/summary.rs:44-110`.

| Field | Use |
|---|---|
| `task_id` | selection key, `id:` label, title fallback, right pill of the details frame |
| `backend_id` | queue places, slots, move group |
| `runner_state` | icon rules (`Provisioning`, `PauseRequested`, `Interrupted`), queue place, move availability, archival removal |
| `state` (`TaskState`) | main input of the icon, label, activity time, readiness |
| `holds_slot` | slots count |
| `generation` | sent as `expected_generation` in a move |
| `revision` | event ordering; makes the detail reads due |
| `rank` | written by `apply_order`; not displayed |
| `created_at`, `as_of` | elapsed time on the status row |
| `state_since` | activity age |
| `workflow_id`, `workflow_version` | status row `{workflow_id} v{version}` |
| `runner` | `problem`, `execution_cursor.current_state/current_action_type/current_decision_by`, `execution_status`, `observed_at`, `pending_input`, `event_cursor`, `default_harness_unavailable{harness, kept}` |
| `controller` | `freshness` (`Live`/`Stale`), `last_heartbeat_at` |
| `inputs` | to filter answered attention items after a `RunnerReported` event |
| `operator_commands` | patched by events; the verdict notification |
| `attention[]` (`kind`, `since`, `waiting_id`) | icon, activity age |
| `queue` (`reason`, `since`, `message`) | blocked icon, slots, queue text |
| `workspaces[]` | workspace rows |
| `ticket`, `title` | row lines 1 and 2, frame edge |
| `resume` (`exhausted`, `next_attempt_at`, `consecutive_losses`, `last_loss.from`) | loss state |
| `actions`, `panel`, `manual_transitions`, `steps`, `slot_release_pending`, `archive`, `observation` | used by other modules (action panel, answer panel, Info) |

### State icon — the exact rule

Helper 1 — `loss_state(runner_state, resume)` (`crates/narsor-protocol/src/display.rs:52-69`):
- `runner_state != Interrupted` → none.
- `resume.exhausted` is set → `Exhausted(reason)`.
- `resume.next_attempt_at` is set → `AutoResume{attempt: max(resume.consecutive_losses, 1), limit: 5}` (`RESUME_ATTEMPT_LIMIT = 5`).
- `resume.last_loss.from == PauseRequested` → `LostWhilePausing`.
- else none.
- `needs_owner()` is true for `Exhausted` and `LostWhilePausing`, false for `AutoResume`.

Helper 2 — `attention_of(task)` (`list.rs:220-246`):
- **Problem** when any of: `task.runner.problem` is set; `loss_state(...)` needs the owner; `task.attention` contains a kind in {`Problem`, `FailedStep`, `ProvisioningFailed`}.
- else **Question** when `task.attention` contains a kind in {`Question`, `Approval`, `ConductorProposal`}.
- else **None**. (Kinds `EntryLimit` and `Unrecognized` do not make attention here.)

Helper 3 — `controller_offline(task)` (`list.rs:339-343`): `task.controller` is `None` or `controller.freshness != Live`.

`state_icon(task)` (`list.rs:254-303`) — the first rule that holds wins:

| # | Condition | Icon | `StateIcon` | Word (`word()`, `list.rs:170-187`) | Role |
|---|---|---|---|---|---|
| 1 | `state == Archived` | `▪` | Archived | `archived` | Done (grey) |
| 2 | `attention_of == Problem` OR `state == Error` | `!` | Problem | `problem` (on the status row: `failed` when `state == Error`) | Problem (red), bold |
| 3 | `attention_of == Question` OR `state == HumanNeeded` | `?` | NeedsReply | `awaiting input` | Attention (yellow), bold |
| 4 | (`state == Queued` AND `queue.reason` ∈ {`ControllerOffline`, `BackendUnknown`}) OR ((`state == Running` OR loss is `AutoResume`) AND `controller_offline`) | `⊘` | Blocked | `blocked` | Calm |
| 5 | `state == Finished` | `✓` | Done | `done` | Done |
| 6 | `state == Paused` AND `runner_state == PauseRequested` | `◑` | Pausing | `pausing` | Calm |
| 7 | `runner_state == Provisioning` | `⟳` | Starting | `starting` | Live (green) |
| 8 | loss is `AutoResume` | `↺` | Resuming | `resuming` | Calm |
| 9 | `state == Paused` | `‖` | Stopped | `paused` | Calm |
| 10 | `state == Queued` | `◷` | Queued | `queued` | Calm |
| 11 | `state == Running` | `●` | Running | `running` | Live |
| 12 | `state == Unrecognized` | `·` | Unknown | `unknown` | Done |

Client-only icons: `○` New (`new`, reserved, not used), `↻` Sending (`sending`, Calm; pending row).

"Waits for your reaction" = `StateIcon::needs_operator()` = icon is `?` or `!` (`list.rs:164-166`). The header counters, the bold look and `Space` use this same test.

### Selection (`list.rs:556-570`, `:1017-1072`)

- The selection is a `task_id` (not an index). A refresh, a patch or a move never changes which task is selected.
- Initial: no selection → the first shown row.
- `j`/`↓`, `k`/`↑`: `step(±1)`, clamped at the ends. `g` first, `G` last.
- If the selected task leaves the shown rows (removed, hidden by `z` or by the filter): the new selection is the first shown row that was **below** it in the old order, else the nearest shown row **above**, else the first row (`settle_selection_with`).
- If the selected task left the list (not only hidden), a Warning shows: `{title}: zniknęło z listy (zarchiwizowane)` + ` — zaznaczono {title of new selection}` when one exists (`mod.rs:1535-1557`).
- The first visible row (`top`) is kept between frames, so a refresh does not scroll the list.
- A selection change puts the *Przebieg* focus back on the axis and aborts the detail reads of the previous task (exception: an explicitly opened finished task keeps its reads) (`change_list`, `mod.rs:1019-1041`).

### `Enter` on the list (`open_selected`, `panel/app.rs:57-72`)

Focus → `Details`; `details_opened = true` for the task; if the task has an answer panel, the panel unfolds and takes the keys. `Tab` on the list only moves the focus to the details (`mod.rs:919-921`).

### Filter `/` (`list.rs:684-754`, `mod.rs:952-962`)

- `/` starts editing (the old filter is remembered). Characters (no modifier, or Shift only; no control characters) append. `Backspace` removes the last character. `Enter` keeps the filter and leaves the edit. `Esc` restores the filter from before the edit.
- Match rule (`matches_filter`): filter empty → all rows match. Else `needle = filter.to_lowercase()`; the row matches when the lowercase **full `task_id`**, the lowercase **title** (`title_of`: title or task id) or the lowercase **ticket** contains the needle (substring). Pending rows match on title and task id.
- The filter is applied at each key. The order of rows does not change.
- No key clears a kept filter; to clear it, press `/`, delete the text, `Enter`.

### Hide done `z` (`list.rs:305-308`, `:997-1001`)

- Toggle. Hides tasks with `state == Finished` or `state == Archived`.
- The title shows ` · ✓ ukryte {n}`, where `n` = count of all such tasks (independent of the text filter).

### `Space` — next waiting (`select_next_waiting`, `list.rs:969-993`)

- Scope: the shown rows (after `z` and `/`; pending rows excluded).
- Start at the row after the selected one, wrap round, select the first task whose `state_icon(task).needs_operator()` is true. The search includes the selected row last.
- None found → Warning `żadne zadanie nie czeka na Ciebie` (`mod.rs:912-916`).
- `Space` only moves the selection. It does not open the details.

### Pending rows of new tasks (`list.rs:511-552`)

| `PendingState` | Icon | Label (line 3) | Line 1 | Line 2 |
|---|---|---|---|---|
| `Sending` | `↻` (Calm) | `wysyłanie…` | `id:{short id}` | the title |
| `NotSent` | `!` (Problem, bold) | `nie wysłano` (Problem) | `id:{short id}` | the title |

- A pending row is added at the end and selected (`push_pending`). It disappears when a list read contains the same `task_id` (`replace`, `list.rs:1021-1025`).
- `r` on the list = `Action::RetrySend` → `retry_selected()` (in `new_task/mod.rs:1267`) → `Request::SendTask(id)`.
- The loop sends again what a lost answer left: `retry_outgoing_if_due` / `next_outgoing_due` (`new_task/mod.rs:1237-1258`).
- Docs and `mod.rs:90` write `✗ nie wysłano`; the list row code uses the `!` icon (`StateIcon::Problem`).

### Queue move (`mod.rs:1066-1135`, `:1261-1308`; `list.rs:1247-1345`)

Availability (decided on the client): `task.runner_state == Queued`. Else Warning `przesuń: zadanie nie jest w kolejce` (`actions.rs:173-176`, `mod.rs:1071-1075`). The mutation gate applies too.

**`Ctrl+↑` / `Ctrl+↓`** (`step_move`): one place, relative to the neighbour row of the same backend among the shown rows.
- No neighbour → Warning `przesuń: to zadanie jest już pierwsze w kolejce` / `… ostatnie w kolejce`.
- Sends at once (no confirmation): target `Before(neighbour id)` for up, `After(neighbour id)` for down.

**`m`** (move mode, context `Move`):
- `j`/`k` (`↓`/`↑`) change the preview slot inside the shown rows of the same backend (`step_move`, clamped). The row is drawn at the preview place with the label `↕ {slot+1}/{group size}`.
- `Enter` (`finish_move`): preview unchanged → nothing is sent. Moved down (`slot > origin`) → `After(row above the new place)`. Moved up → `Before(row at the new place)`.
- `Esc` cancels. The mode is cancelled automatically when the task leaves the list or is no longer `runner_state == Queued` (`cancel_move_if_gone`).
- If the mode could not start: Warning `przesuń: zadanie nie jest na liście`.

**API call**: `client.control_move_task(task_id, &MoveTaskRequest{ target, expected_generation: Some(task.generation) })` → `POST /control/v1/tasks/{task_id}/move`. `MoveTaskTarget` = `Before(id) | After(id) | Top | Bottom` (the TUI sends only `Before`/`After`).

- The client does not reorder locally. The new order arrives as a `Moved` event.
- Success notification: `{title}: przesunięto {przed|za} {anchor title}` (for `Top`/`Bottom`: `na początek` / `na koniec`).
- Failure (Error): `{title}: przesunięcie odrzucone — {reason}` + `; lista wczytana na nowo` when the error is not a transport error (then the list is read again and this text is shown again after the read).

### Error reasons (`failure_reason`, `mod.rs:1656-1666`)

| `ClientError` | Text |
|---|---|
| `Refused`, `Request`, `Degraded` | the server message (one line) |
| `Conflict` | `zadanie zmieniło się w międzyczasie (409)` |
| `NotFound` | `nie ma takiego zadania` |
| `Unavailable` | `brak połączenia z serwerem` |
| other | the error text |

---

## A.4 Details frame and header

Source: `detail.rs:1032-1404` (`render`, `render_view`), `:1556-1604` (`header`).

### Empty states

- No selected task: frame ` Szczegóły `, text `brak zaznaczonego zadania` (`detail.rs:1069-1076`).
- Selected pending row: frame with the title; content from `new_task::render_pending` (other part).
- Finished task without a runner, not yet opened with `Enter` (`requires_explicit_open` = `state == Finished && runner.is_none()`, `detail.rs:109-111`): the header is drawn, then `więcej danych pojawi się po wejściu w zadanie (Enter)` (Attention). No tabs. No detail reads start until `Enter`.
- The same task after `Enter`, timeline not yet read, read in progress: `wczytuję dane szczegółowe…`.

### Frame top edge (`edge_layout`, `detail.rs:943-1015`)

- Left: pills `{ticket}` and `{title}` (each only when not blank). When both are missing: plain text ` {task_id} `.
- Right: pill with the short task id (first 11 characters). It is never cut.
- Cut order: the title first (min 2 cells), then the ticket (min 3 cells), then they are removed.
- Focused frame: `▸` before the left side, bold. Loading marker (spinner glyph) after the left side while a detail read of the task runs.
- When the edge cuts the title (`title_is_cut`), the header's first line shows the title in full (bold) followed by two spaces and the full `task_id` (`title_line`, `detail.rs:1617-1642`).

### Header lines, in the order of the code

1. (only when the title is cut) the title line.
2. One row for each workspace (`task.workspaces`, in the workflow's order).
3. The status row.
4. (only when usage was reported) the task usage row.
5. (only when present) the notice line.
6. A rule; then the tab bar; then a second rule with the active-tab marker.

### Workspace rows (`src/tui/task_app/workspace_rows.rs`)

Format (columns aligned between rows; an empty column in one row is kept as blanks):

```
{name}: {repo} · ⎇ {branch} → {base_branch} · +{added} −{removed} · {PR}
```

- `name` = `workspace.name` + `:`. `repo` = `workspace.repo`.
- Branch text (`branches`, `:121-130`): `{branch} → {base}` when both exist and differ; only `{branch}` when they are equal or no base; only `{base}` when no branch; empty when both are missing. Prefix `⎇ `.
- Changes: shown when `added` or `removed` is set: `+{added or 0} −{removed or 0}` (the minus is U+2212 `−`). `+N` in the Added role (green), `−M` in the Removed role (red).
- Pull request (`pr_label`, `crates/narsor-protocol/src/display.rs:132-143` + `workspace_rows.rs:81-94`):
  - number = `workspace.pr` trimmed, leading `#` removed; if missing, the last path segment of `pr_url` when it is all digits; shown as `#{number}`;
  - url = `workspace.pr_url` only when `safe_link_url` accepts it (absolute `http`/`https`, has a host, no whitespace or control characters);
  - url present → `{#number | PR} ↗` as a link (Link role: blue, underlined);
  - number without url → `#{number}` plain text;
  - none → empty (the code shows no `—`).
- Separators: one space between name and repo; ` · ` before branch, changes, PR — only where the row has text for that column.
- Narrow width, in this order (`Columns::fitted`, `:144-177`): 1. remove the base branch; 2. remove the changes column; 3. cut the branch (min 2 cells); 4. cut the repository (min 2 cells). The name and the PR are never cut.
- Hyperlinks: the PR text becomes an OSC 8 link only when the terminal supports it (`hyperlinks_supported`, `detail.rs:459-484`) and the URL is `http(s)` with printable ASCII only. A web UI can use a normal `<a>`.
- `o` (open link) and the "Otwórz link" action use `openable_links(task)` and `open_link` in `actions.rs:1306` (other part). When no link exists: `link: zadanie nie ma jeszcze linku do PR` (`actions.rs:177-182`).
- The header has **no** ticket link (no "Jira … ↗"). The ticket is only text on the frame edge.

### Status row (`status_line`, `detail.rs:1644-1688`; `status_parts`, `:1838-1875`)

```
{icon} {word} · {part} · {part} …
```

- `{icon}` = `state_icon(task)` glyph. `{word}` = `icon.word()`, except `failed` when the icon is Problem and `task.state == Error` (`status_word`, `:1828-1834`). Role = the icon's role.
- Parts when `task.state == Queued`:
  1. `queue_text(task, queue_place)` (Calm);
  2. `{workflow_id} v{workflow_version}` (Done).
- Parts in all other states:
  1. step (Calm): `krok {n} · {icon name}` when the timeline gives a step number and the runner names a state; `krok {n}` with only the number; `krok {icon name}` with only the name; nothing with neither. `n` = `progress.step_count()` first value (from the timeline; other part). Name = `runner.execution_cursor.current_state` with the step icon.
  2. `{workflow_id} v{workflow_version}` (Done).
  3. elapsed time `short_age(seconds)` (Done) — see below.
  4. only when `state == Running` and the controller is not live (Attention): `kontroler offline {short_age(as_of − last_heartbeat_at)}`; `kontroler offline` when no heartbeat time; `brak kontrolera` when `task.controller` is `None`.
- Display order: the parts are sorted by role — Attention first, then Calm, then Done. Separator ` · ` (grey).
- Narrow width: a Done part that does not fit is skipped; another part is cut with `…` and the row ends.

`queue_text` (`detail.rs:1908-1937`); `#{place}` is the client queue place, else `position` from the server:

| `task.queue.reason` | Text |
|---|---|
| `WaitingForCapacity{position, running, limit}` | `#{place} · czeka na slot ({running}/{limit})` |
| `ControllerOffline` | `kontroler offline {short_age(as_of − queue.since)}` |
| `Admitted` | `[#{place} · ]przyjęte, czeka na start` |
| `BackendUnknown` | `backend nieznany` |
| `queue` is `None` | `[#{place} · ]w kolejce` |

Elapsed time on the status row (`elapsed`, `detail.rs:1879-1889`): `until − task.created_at`, where `until` = `runner.observed_at` when `runner.execution_status.is_terminal()`, else `task.as_of`. Format `short_age` (one unit: `40s`, `14m`, `2h`, `3d`). This is the whole life of the task, computed from server timestamps (it does not tick between reads).

### Task usage row (`src/client/task_usage_text.rs:49-150`, `detail.rs:1164-1166`, `:1606-1615`)

- Input: the timeline response; `sum(timeline.positions)` adds `position.usage` of all positions. `None` when no position reported usage → no row.
- Fields summed: `total` (tokens in/out), `subagents`, `subagent_count`, `cost_usd`, `conductor_total`, `conductor_cost_usd`. `subagent_cost_usd` is always `None` today.
- Format (`line`, `:97-118`), groups joined with ` · `, a group or part that nobody reported is left out (no placeholder):

```
razem ${agents cost + conductor cost} · ✨ ${cost} ↑{in} ↓{out} (◈ {count} ${cost} ↑{in} ↓{out}) · ⚡ ${cost} ↑{in} ↓{out}
```

  - `razem` = the sum of agent cost and conductor cost (present when at least one exists).
  - `✨` = agents (their numbers include the subagents').
  - `(◈ …)` = subagents; shown only when their tokens `input + output > 0` (or a cost exists); the count only when `> 0`.
  - `⚡` = conductor.
- Money: `${:.2}` (two decimals). Tokens: `↑{scaled(input)} ↓{scaled(output)}`.
- `scaled(count)` (`:228-248`): `< 1000` → the number; `< 999 500` → thousands with `k`; else millions with `M`. Inside a unit: quotient ≥ 10 → rounded integer (`120k`); else one decimal, rounded half up, without `.0` (`1.5k`, `2k`, `2.4M`).
- Width: the full line; if too wide, the line without the subagents block; if still too wide, cut with `…`.
- Example from the test (`:319-327`): `razem $68.30 · ✨ $34.15 ↑304 ↓72k (◈ 2 $12.00 ↑100 ↓20k) · ⚡ $34.15 ↑304 ↓72k`.

### Notice line under the header (`notice`, `detail.rs:1801-1823`)

First match wins:

1. `loss_state(task.runner_state, &task.resume)` exists → `describe_loss` (`list.rs:191-204`); role Problem when the owner must act, else Attention:
   - `AutoResume` → `wznowienie automatyczne, próba {attempt}/{limit}`
   - `Exhausted(reason)` → `nie udało się wznowić automatycznie: {detail}, uruchom od nowa (Ctrl+.)`; `{detail}` = the reason without the prefix `could not resume automatically: `; when it equals `the workload was lost` → `runner utracony` (`exhaustion_detail`, `list.rs:209-216`)
   - `LostWhilePausing` → `utracone podczas wstrzymywania, czeka na Ciebie (Ctrl+.)`
2. `task.runner.default_harness_unavailable` exists → `{harness} niedostępny tutaj — zostaje {kept}` (Attention).
3. Else no line.

### Tab bar (`tab_bar`, `detail.rs:1408-1450`; rule label `:1266-1274`)

- Names in the order of `DetailTab::ALL`: `Rozmowa  Przebieg  Zdarzenia  Dyrygent  Info`. The active tab is bold, in the Focus role when the details content has the keys, else Calm. Inactive tabs are grey.
- The rule under the names has a heavy segment (`━`) under the active tab.
- On *Przebieg* with at least one position, the right end of that rule shows `↧ podąża [f]` (follow on, green) or `· ręcznie [f]` (follow off, grey).

### "Not started" and archived message (`unstarted_message`, `detail.rs:442-454`)

Shown in place of the content of *Przebieg*, *Rozmowa*, *Zdarzenia* and *Dyrygent* (not *Info*):

| `readiness(task)` (`detail.rs:430-438`) | Text |
|---|---|
| `Archived` (`state == Archived`) | `storage usunięty przy archiwizacji` |
| `NotStarted` (`runner` is `None` AND `state == Queued`) | `jeszcze nie wystartowało, #{place} w kolejce` (or `jeszcze nie wystartowało, w kolejce` when no place) |
| `Started` | no message |

For these two cases no detail read starts (`detail.rs:699-701`).

### Content and panel geometry (`areas`, `detail.rs:515-547`)

Header lines + rule, 2 rows for the tabs, the content, and at the bottom the answer panel (or the action panel, which takes the same place). The content keeps at least 1 row. The panel height comes from `WaitingPanel::preferred_height` or `actions::panel_height` (other part).

---

## A.5 Footer (last row) (`render_bar`, `layout.rs:464-527`)

- Left: key hints of the current context (`keymap::hint_line`).
- Right: the notification while it is visible; else the usage limits; else nothing. The right side takes the room it needs; the hints get the rest and drop from the end.
- Quit confirmation: only `q wyjdź  Esc anuluj`. View that opens: only `Esc wróć  q wyjdź`.

---

## A.6 Usage limits in the footer

Source: `src/tui/task_app/usage.rs:55-228`, `src/client/usage_text.rs:15-36`.

- Read: `client.control_usage()` → `GET /control/v1/usage` → `HarnessUsageResponse{ harnesses: [HarnessUsageView{ harness, limits: [UsageLimit{ window, utilization, resets_at, status }] }] }`.
- Poll: at start, then `USAGE_PERIOD = 15 s` after the previous read **finished** (success or failure). Independent of `tui_refresh_interval` (also runs with `none`). No backoff.
- A successful answer replaces the state. A failure clears it (nothing is shown). An answer with no harnesses is no state. A failure does not change the connection state. This read does not animate the spinner.
- Format:

```
{short name} {window} {percent}% ↻{time} · {window} {percent}% ↻{time} | {short name} …
```

  - Harness short name (`crates/narsor-protocol/src/harness.rs:34-50`): `claude`, `codex`, `opencode`, `copilot`; an unknown harness id is shown as is.
  - Between limits of one harness: ` · `. Between harnesses: ` | `. Order as the server sent.
  - `percent(utilization)` = `round(utilization × 100)` clamped to 0–100; NaN → 0.
  - `time_to_reset(now, resets_at)`: whole minutes, rounded down, min 0. ≥ 1 day → `{d}d{h}h`; ≥ 1 hour → `{h}h{m}m`; else `{m}m`. Past reset → `0m`. Examples: `4h10m`, `2d3h`, `1d0h`, `1h0m`, `45m`.
  - `now` is the client clock at each redraw.
- Colour by `limit.status`: `AllowedWarning` → Attention (yellow, bold); `Rejected` → Problem (red); `Allowed`, `Unrecognized`, none → plain. With `NO_COLOR`, a `!` stands before the window for the two coloured cases (`!5h 95% ↻1h0m`).
- Truncation: while more than one harness is left and the text is too wide, remove the **last** harness. One harness still too wide → cut at the end with `…`.
- Example: `claude 5h 1% ↻4h10m · 7d 90% ↻2d3h | codex 5h 12% ↻3h0m`.

---

## A.7 Notifications

Source: `src/tui/task_app/notify.rs`.

- One slot. A new notification replaces the old one. `LIFETIME = 6 s`.
- Text is made one line. Too long → cut with `…` (`layout.rs:521-523`).
- Kinds:

| `Severity` | Icon | Role |
|---|---|---|
| `Success` | `✓` | Live (green) |
| `Warning` | `!` | Attention (yellow) |
| `Error` | `✗` | Problem (red) |

- Display: `{icon} {text}`.
- Counted notifications (`push_counted`): when a notification of the same kind key is still visible, the count rises and the "many" text is shown. Used only for auto-resume (key `auto-resume`).
- `notify_after_refresh` shows a text now and again after the next list read (so the reason of a refused write stays the last word) (`mod.rs:1424-1434`).
- `after_work_check`: after work was sent to a task that had no slot, the list is read again; when the task is then `state == Queued`, the same text is shown again with the suffix ` — w kolejce, czeka na slot` (`mod.rs:1402-1452`, `actions.rs:1337`).

### Notification texts found in Part A files

| Text | Severity | Where |
|---|---|---|
| `{state}[ ({age})]: akcja zmieniająca stan jest zablokowana` | Warning | `connection.rs:191-194` |
| `zakończ bieżący formularz przed zmianą widoku` | Warning | `mod.rs:886` |
| `żadne zadanie nie czeka na Ciebie` | Warning | `mod.rs:914` |
| `przesuń: zadanie nie jest w kolejce` | Warning | `mod.rs:1073` + `actions.rs:175` |
| `przesuń: to zadanie jest już pierwsze w kolejce` / `… ostatnie w kolejce` | Warning | `mod.rs:1088` |
| `przesuń: zadanie nie jest na liście` | Warning | `mod.rs:1121` |
| `{title}: przesunięto przed {anchor}` / `za {anchor}` / `na początek` / `na koniec` | Success | `mod.rs:1268-1283` |
| `{title}: przesunięcie odrzucone — {reason}[; lista wczytana na nowo]` | Error | `mod.rs:1293-1297` |
| `{title}: wznowiono automatycznie po restarcie kontrolera (poza limitem {limit} prób)` (attempt = 0) | Warning | `mod.rs:1511-1514` |
| `{title}: wznowiono automatycznie po utracie runnera (próba {attempt}/{limit})` | Warning | `mod.rs:1516-1519` |
| `wznowiono automatycznie {count} {zadanie|zadania|zadań}` (several in a row) | Warning | `mod.rs:1521` |
| `{title}: nie udało się wznowić automatycznie: {detail}` | Error | `mod.rs:1526-1529` |
| `{title}: zniknęło z listy (zarchiwizowane)[ — zaznaczono {title}]` | Warning | `mod.rs:1548-1556` |
| `strumień zdarzeń zakończony: {error}` | Error | `mod.rs:1624` |
| `{label}: krok zniknął z przebiegu — fokus wrócił na oś` | Warning | `detail.rs:386-391` |
| `to zadanie na nic nie czeka` (`Tab` in details, no panel) | Warning | `panel/app.rs:84` |
| `runner nie odpowiada — spróbuję ponownie przy następnej zmianie` (text inside a tab, not on the bar) | — | `detail.rs:565` |

Plural rule `tasks_word` (`mod.rs:1636-1642`): 1 → `zadanie`; last digit 2–4 and last two digits not 12–14 → `zadania`; else `zadań`.

---

## A.8 Refresh model

### What is read, and when

| Read | Client methods (endpoint) | Trigger / interval |
|---|---|---|
| List refresh (`start_refresh`, `mod.rs:1324-1340`) | `control_tasks_archived(false)` (`GET /control/v1/tasks?archived=false`) and `harness_selection()` (`GET /settings/agent-harness`) in parallel; then `control_capabilities()` (`GET /control/v1/capabilities`) once, only while capabilities are not yet read and the list read succeeded | at start; every `tui_refresh_interval`; `DEBOUNCE` after an event that asks for a read; after a refused or lost write (`refresh_after_write`); retry after a failure |
| Usage limits | `control_usage()` (`GET /control/v1/usage`) | at start, then 15 s after the previous read finished |
| SSE stream | `follow_control_events(Some(cursor), None)` (`GET /control/v1/events/stream?cursor=…`) | opened once a list read gave a cursor |
| Timeline of the selected task | `control_timeline(task_id)` (`GET /control/v1/tasks/{id}/timeline`) | when due (first show; row revision changed) — always first, the other detail reads wait for it |
| Events of the selected task | `control_execution_events(task_id, &ExecutionEventsQuery{after, limit: Some(PAGE_LIMIT)})`, up to `events::MAX_PAGES` pages, each bounded by `RELAYED_READ_LIMIT = 3 s` | tabs *Przebieg*, *Rozmowa*, *Zdarzenia*, when due |
| Attempt conversations | `conversation::read_attempt(client, task_id, attempt_id, from)` | *Przebieg*: attempts of the selected step; *Rozmowa*: all attempts of positions with `conversation == Recorded` |
| Conductor state | `start_conductor_read` (other part) | every tab, when due; made due after **every** list read |
| New task resend | `retry_outgoing_if_due` | when the outbox has a due item |

Rules (`detail.rs:682-749`):
- Detail reads run only for the **selected** task and the **visible** tab. No read starts for a task with `readiness != Started`, or for a finished task without a runner that was not opened with `Enter`.
- One job per key at a time (`background.rs:213-226`); a key that runs is not started again.
- `invalidate_changed` (`mod.rs:274-289`): after each list read, for each task that has a details view: if `task.revision` differs from the revision before the read → the timeline, attempts and events become due; the conductor read becomes due always.
- A read that the runner must answer and that takes more than 3 s gives the tab the note `runner nie odpowiada — spróbuję ponownie przy następnej zmianie` and does not change the connection state (`detail.rs:557-577`).
- A failed detail read stays as text in its tab. Only `Unavailable` / `Server` errors count against the connection (`settle_with`, `detail.rs:650-662`).
- Views of tasks that left the list are forgotten (`DetailState::retain`).

### Schedule constants (`src/tui/refresh.rs`)

- `tui_refresh_interval`: default `DEFAULT_TUI_REFRESH_SECS = 10` s (`crates/narsor-shared/src/config/settings.rs:99`); `none` turns the periodic read off (`Option<Duration>`).
- `DEBOUNCE = 400 ms`: an event that asks for a read sets `requested` (only the first request in a window counts); the read runs 400 ms after that first event.
- Failure backoff: `FIRST_RETRY = 1 s`, doubled per consecutive failure, `MAX_RETRY = 10 s` (1, 2, 4, 8, 10, 10 …). A success resets it. The header countdown `ponawiam (Ns)` shows this delay.
- `next_due` = the minimum of (request + 400 ms, last + interval, retry time). No new refresh starts while one runs.
- A write that asks for a read while a refresh runs sets `refresh_again`; one more read follows the running one (`mod.rs:1373-1379`).

### What a list read does (`TaskApp::load`, `mod.rs:1456-1479`)

1. `cursor = listing.cursor`; connection → Live; schedule reset; `stream_failures = 0`; `stream_ended = false` (the feed opens again).
2. Replace the tasks (`TaskList::replace`): pending rows whose id is now on the server list are dropped; the selection stays on its `task_id`.
3. Forget detail views of tasks that left; mark changed tasks due.
4. `sync_waiting()` (answer panel), `sync_overlay()` (action panel — a refresh does not close it), `settle_outbox()`.
5. Announce the removal of the selected task, if it left.

The harness snapshot is taken only together with a successful list read; a failed harness read keeps the last value (`mod.rs:1355-1357`).

### SSE stream

- Frame type: `ControlEventEnvelope{ cursor, event: TaskEvent{ sequence, task_id, at, runner_state, generation, kind } }` (`crates/narsor-protocol/src/controller/task/events.rs:146-161`).
- The cursor of each applied event becomes the application cursor (`mod.rs:1598-1600`). The stream always resumes from the last cursor.
- Inside `follow_control_events` (`crates/narsor-api-client/src/api/client.rs:1139-1398`):
  - reconnect delay `reconnect_delay(failures)`: 0 for the first attempt; then `FIRST_RECONNECT_DELAY = 250 ms` doubled per failure; `MAX_RECONNECT_DELAY = 15 s`.
  - `Unavailable` / `Server` error → the error is yielded (the UI shows `ponawiam`), then reconnect from the cursor.
  - The server closed the stream (token expiry, proxy): reconnect at once when the connection delivered at least one event; with a growing delay when it delivered none.
  - Other errors (`Unauthorized` after a token re-read, `Forbidden`, `NotFound`, invalid cursor / `Request`) → yielded once, the stream ends.
- In the application (`on_stream`, `mod.rs:1591-1632`):
  - event → connection Live, `stream_failures = 0`, apply to the list;
  - transport error → `stream_failures += 1`, state `Retrying` with `reconnect_delay(stream_failures)`;
  - other error or end → `stream_ended = true`, `Disconnected`. The next successful list read (periodic or retry) clears `stream_ended` and the feed opens again from the new cursor (`EventFeed::ensure_open`, `mod.rs:1694-1710`).

### How an event patches the list (`TaskList::apply_event`, `list.rs:1098-1191`)

Guards:
- Task not on the list → nothing patched, **read requested**.
- `event.sequence < task.revision` → dropped (not applied, the cursor does not move).
- `event.sequence == task.revision` → dropped, except `Moved`: the order is applied.

Always patched (when newer): `task.revision = event.sequence`, `task.runner_state = event.runner_state`, `task.generation = event.generation`; when `runner_state != Queued` → `task.queue = None`.

If `event.runner_state` is `Archiving` or `Archived` → the row is removed at once (selection settled; move mode cancelled).

By event kind:

| `TaskEventKind` | Direct patch | Read requested |
|---|---|---|
| `Moved{order}` | rows of the task's backend reordered to `order`; `rank = position + 1`; tasks not named go last (`apply_order`, `list.rs:1216-1241`) | no |
| `QueueChanged{reason}` | when `runner_state == Queued`: `task.queue = {reason, message: reason.describe(), since: event.at}` | no |
| `InputQueued{pending_input_id}` | `task.attention` items with that `waiting_id` removed | no |
| `RunnerReported{execution_status, pending_input_id, attention, event_cursor}` | `runner.execution_status`, `runner.event_cursor`; `runner.pending_input = None` when the id is none; `task.attention` = items of kind `ProvisioningFailed` kept + reported items whose `waiting_id` has no input with status `Queued`/`Delivered`/`Consumed`, sorted by `since` | only when the status, the pending input id or the attention changed; always when `task.runner` is `None` |
| `OperatorCommandQueued{command_id, command}` | the command is added to `task.operator_commands` with status `Queued` | no |
| `OperatorCommandResolved{command_id, status, detail}` | the status/detail of the known command updated; a verdict notification (`notify_resolved`, `actions.rs:1327`) when it was not settled before | when the command is unknown or `status == Applied` |
| `AutoResumed{attempt, limit}` | `resume.next_attempt_at = None`; notification | yes |
| `AutoResumeExhausted{reason}` | `resume.next_attempt_at = None`; `resume.exhausted = {reason, at: event.at, announced: true}`; notification | yes |
| all other kinds (`Created`, `OwnerCommandAccepted`, `ControllerReported`, `ResumeMessageStored`, `ResumeMessageTaken`, `PriorityChanged`, `InputsUpdated`, `MaterialChanged`, `SlotReleaseScheduled`, …) | none | yes |

In addition: a read is requested whenever `runner_state` changed (`list.rs:1189`), because `task.state` and `holds_slot` come only from a read.

Consequences for a second client:
- `task.state` (the main input of the icon) is never patched from events. The icon changes fully only after the debounced read (400 ms).
- The current step of a running task (`runner.execution_cursor`) is not in the events. It refreshes with the periodic read.
- After each event: `sync_waiting()` and `sync_overlay()` run.

### What a refresh never changes

Selection (by `task_id`), focus, tab, the list scroll position, the filter, `z`, the move mode (unless the task is gone or not queued), cursors and follow state of each task's tabs (`DetailState.views`, kept per task id), drafts (`DetailState.drafts`, per task id), the open overlay/forms.

---

## A.9 API client calls made from Part A files

| Method | Arguments | Trigger | Where |
|---|---|---|---|
| `control_tasks_archived(false)` | — | list refresh | `mod.rs:1329` |
| `harness_selection()` | — | list refresh; `h` | `mod.rs:1330`, `background.rs:143` |
| `control_capabilities()` | — | first successful refresh; `n`/`R` when not yet read | `mod.rs:1333`, `background.rs:111` |
| `control_tasks_archived(true)` + `control_history()` | — | `H` | `background.rs:118-121` |
| `control_repositories()` | — | `R`, `n` | `background.rs:125`, `:135` |
| `control_workflows()` | — | `n` | `background.rs:134` |
| `control_move_task(task_id, &MoveTaskRequest{target, expected_generation: Some(generation)})` | target `Before(id)`/`After(id)` | `Ctrl+↑`/`Ctrl+↓`; `Enter` in move mode | `mod.rs:1261-1266` |
| `follow_control_events(Some(cursor), None)` | cursor of the last applied event or last read | feed closed and a cursor exists | `mod.rs:1703-1708` |
| `control_usage()` (through `UsageSource`) | — | every 15 s | `usage.rs:93-102` |
| `control_timeline(task_id)` | — | selected task, timeline due | `detail.rs:763-775` |
| `control_execution_events(task_id, &ExecutionEventsQuery{after, limit})` | paged by the last `sequence` while `has_more` | selected task, events due | `detail.rs:804-848` |
| `conversation::read_attempt(client, task_id, attempt_id, from)` | — | selected task, attempt due | `detail.rs:778-800` |

Other requests (`SendTask`, `ValidatePath`, `Recover`, `AddRepository`, `RemoveRepository`, `Answer`, `Act`, `SetLimit`, `SetHarness`, `ReadBranches`) are dispatched in `mod.rs:1152-1173` and implemented in other modules. Each mutation passes `mutation_allowed()` and carries a new `idempotency_key()` = UUID v4 (`mod.rs:419-421`).

---

## A.10 Colour roles (`src/tui/style.rs:59-185`)

| Role | Meaning | Colour (with colour) | Without colour |
|---|---|---|---|
| Attention | needs the operator | yellow, bold | bold |
| Problem | problem, error, irreversible | red | bold |
| Live | happening now, success | green | plain |
| Calm | calm states, normal text | terminal default | plain |
| Done | finished, secondary, stale, inactive | dark grey | dim |
| Focus | focus and interaction | cyan | bold |
| Conductor | conductor | brown (ANSI 130) | plain |
| Link | link | blue, underlined | underlined |
| Added / Removed | `+N` / `−M` | green / red | plain |

Other looks: focused panel frame = cyan + `▸` before a bold title; other frames grey. Selection = thin rules above and below plus a bar at the left (task list: the bar only). Key caps on the bar: black on cyan. Spinner frames: `⠋ ⠙ ⠹ ⠸ ⠼ ⠴ ⠦ ⠧ ⠇ ⠏` (ASCII fallback `| / - \`), one frame per tick (250 ms) while a read other than the usage read runs (`src/tui/spinner.rs`, `background.rs:253-309`).

---

## A.11 Where `docs/user/tui.md` and the code disagree (trust the code)

1. **Tab order and default tab.** Docs: `Przebieg  Rozmowa  Zdarzenia  Dyrygent  Info`. Code: `Rozmowa, Przebieg, Zdarzenia, Dyrygent, Info`; the default tab is *Rozmowa* (`mod.rs:182-188`, `:236`).
2. **Header row order.** Docs: status row first, then workspace rows. Code: (title line), workspace rows, status row, usage row (`detail.rs:1556-1604`).
3. **Status row format.** Docs example: `● running · krok 3 implement · code-review v3 · 14m` and the mock `running · krok 3/5 · 14m`. Code: `krok {n} · {icon} {name}` (a ` · ` between the number and the name, no `/total`), then workflow, then time; parts are ordered by role (`detail.rs:1859-1864`, `:1662-1667`).
4. **Ticket link.** The docs mock shows `Jira NAR-12 ↗   PR !482 ↗` in the header. The code has no ticket link; the ticket is text on the frame edge. The PR text is `#{n} ↗` (or `PR ↗` without a number), not `PR #42 ↗`.
5. **No pull request.** Docs: `—` when there is none. Code: the column is empty.
6. **Task usage row.** Docs: `razem $1.25 · agenci $1.20 ↑120k ↓8k (subagenci ↑40k ↓3k ×2) · dyrygent $0.05 ↑9k ↓1k`, a missing part reads `—`. `docs/user/conductor.md`: `· dyrygent $0.05 · razem $1.25`. Code: `razem $… · ✨ $… ↑… ↓… (◈ n $… ↑… ↓…) · ⚡ $… ↑… ↓…`; a missing part is left out (`task_usage_text.rs:90-118`).
7. **Follow label.** The `detail.rs` module comment shows `↧ śledzi [f]`. Code: `↧ podąża [f]` / `· ręcznie [f]` (`detail.rs:1269-1272`).
8. **State words.** Docs examples `⊘ blocked`, `↺ resuming` agree. Not in the docs: `?` → `awaiting input`, `‖` → `paused`, `!` → `problem` or `failed` (when `state == Error`), `✓` → `done`.
9. **Quit.** Docs: `q` asks when a draft would be lost. Code: `q` always asks; the draft only adds a line (`mod.rs:965-967`).
10. **Unsent row.** Docs / module comment: `✗ nie wysłano`. Code: icon `!` with the label `nie wysłano` (`list.rs:539-542`).
11. **Third line of a row.** Docs: the third line of a finished, archived, failed or stopped task is "empty or only the time". Code: Finished / Error / Archived have no time; Paused (stopped) and HumanNeeded show the time; only Queued (`#n`) and Running (step) have a label.
12. **Selection look on the task list.** Docs: rules above and below the selected task plus a bar. Code: the task list uses the bar-only variant (`render_barred_list`), so the selection adds no rows (`src/tui/selection.rs:1-36`, `list.rs:39-42`).
13. **Slots.** Docs show `sloty 2/3`. Code: the limit is known only while a task of the backend waits for capacity; else `sloty {n}` without a limit (`list.rs:910-933`).
