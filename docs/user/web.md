# Browser interface

The browser interface does what the [TUI](tui.md) does, in a web browser:
the task queue, the details of a task, answers to what waits for you, the
actions of a task, new tasks, history, repositories, the default harness and
the task limit. It is a client of the same owner API (`/control/v1`) as the
TUI and the CLI.

The API server serves it itself, under `/ui/` on its own address. Nothing
is installed apart from the server, and the build needs no Node toolchain:
the application is plain ES modules compiled into the `narsor-server`
binary (`crates/narsor-web`).

## Opening it

| Server | How |
|---|---|
| The local daemon (trusted mode) | `narsor-cli web` — opens the browser with the token already in the address. `--no-open` prints the address instead. |
| A remote server (network mode) | `narsor-cli --server <URL> web`, when the CLI has a token for that server (`NARSOR_TOKEN`, `NARSOR_TOKEN_FILE` or `NARSOR_TOKEN_COMMAND`, see [CLI](cli.md#global-options)). |
| Any server, without the CLI | Open `<server address>/ui/` and paste a token in the sign-in form. |

`GET /` on the server redirects to `/ui/`.

### Where the token comes from

- **Local daemon:** the `token` field of `daemon.json` in the state directory
  (by default `~/.narsor/daemon.json`). The daemon makes a new token each
  time it starts, so after a restart open the interface again with
  `narsor-cli web`, or paste the new token.
- **Remote server:** an OIDC access token of the installation owner, issued
  by your identity provider. The browser interface has no OIDC sign-in of its
  own yet. When the token expires the interface returns to the sign-in form.

The sign-in form has the same instructions under *Skąd wziąć token?*.

### What happens to the token

- `narsor-cli web` never puts the token in a process argument: the browser
  opener gets a page in the state directory (`web-open.html`, mode `0600`),
  and that page sends the browser on to `/ui/#token=…`.
- A browser never sends the fragment (`#…`) of an address to a server. The
  interface moves the token to the session storage of the tab and removes it
  from the address bar at once. Closing the tab removes the token; the ⏻
  button in the header removes it too.
- Every call carries `Authorization: Bearer <token>`, as for any client. The
  files of the interface are public; they hold no data of the installation.
- The pages go out with a strict content security policy (scripts, styles
  and connections only from the same origin, no inline code, no framing),
  without a referrer and with `nosniff`. Text from agents, runners and
  workflows is always shown as text, never as HTML.

## The screen

- **Header:** connection state (`live`, `ponawiam (Ns)`, `brak połączenia`,
  with the age of the data when not live), the default harness, the slots,
  the counters `?` (tasks that ask) and `!` (tasks with a problem), the usage
  limits of the harnesses, and buttons for history and repositories.
- **Task list:** the queue in server order, with the same state icons as the
  TUI, a text filter, *ukryj ✓* (hide finished tasks) and *następne ?* (the
  next task that waits for you). ▲ ▼ move a queued task.
- **Details:** the workspaces, the status row, the usage row, and five tabs —
  *Rozmowa* (the agent conversation, tool calls folded, subagents grouped),
  *Przebieg* (the steps; a chosen step shows its conversation), *Zdarzenia*
  (the workflow events), *Dyrygent* (the conductor: chat, proposals,
  decisions) and *Info*.
- **Czeka na Ciebie:** the forms the server offers for the task. Choose an
  entry, fill its fields, send. What you type stays when the list refreshes
  and when you look at another task; the browser asks before it closes a tab
  with text that was not sent.
- **Akcje:** what you can do with the task now, as the server offers it
  (pause, resume, select a step, archive …). Irreversible actions say so and
  ask for confirmation.

Below 900 px of width the list and the details are shown one at a time.
The address follows the selection (`#/task/<id>/<tab>`), so a reload comes
back to the same place.

Changes are blocked while the connection is not live, as in the TUI.

## Keyboard

Shortcuts do nothing while the focus is in a text field. `?` shows them.

| Key | Action |
|---|---|
| `j` / `↓`, `k` / `↑` | next / previous task |
| `g` / `G` | first / last task |
| `Space` | next task that waits for you |
| `/` | filter |
| `z` | hide finished tasks |
| `Enter` / `Esc` | open / close the details (narrow screen) |
| `[` / `]` | previous / next tab |
| `.` | actions of the task |
| `n` | new task |
| `K` / `J` | move a queued task up / down |
| `H` | history |
| `R` | repositories |
| `h` | default harness |
| `L` | task limit |
| `u` | refresh now |
| `Ctrl+Enter` | send a message (in a message field) |

## Differences from the TUI

- `u` refreshes the list (`R` opens the repositories, as in the TUI).
- A new task that is being sent is shown in the new-task dialog
  (*wysyłanie…*, *nie wysłano* with *Ponów*), not as a row of the list.
- History lists archived tasks and executions from before the task
  scheduler (with *Odzyskaj*); the steps and conversation of such an old
  execution are not shown — use the TUI or `narsor-cli task history`.
- No drag and drop in the queue; use ▲ ▼ or `K` / `J`.
- No OIDC sign-in in the browser (paste a token).
