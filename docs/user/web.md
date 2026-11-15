# Browser interface

The browser interface does what the [TUI](tui.md) does, in a web browser:
the task queue, the details of a task, answers to what waits for you, the
actions of a task, new tasks, history, repositories, the default harness and
the task limit.

It is served by **`narsor-web`**, a separate program. `narsor-web` serves
the files of the interface and passes the owner API (`/control/v1`) on to a
`narsor-server`, the same API the TUI and the CLI use. `narsor-server`
itself serves no interface. The files are compiled into the `narsor-web`
binary; nothing else is installed and the build needs no Node toolchain.

## Setups

| Setup | `narsor-web` runs | `web_upstream` | `web_token` |
|---|---|---|---|
| Everything on your computer | on your computer | `local` (default): the local daemon, discovered or launched like the CLI and TUI do | `server` (default) |
| Your computer in front of a remote server | on your computer | `https://narsor.example.com` | `server`, with the server's token from `NARSOR_TOKEN_FILE`, `NARSOR_TOKEN_COMMAND` or `NARSOR_TOKEN` |
| Everything on the remote side | next to the server, behind a TLS proxy | the server's address | `passthrough`: every browser presents its own token |

```bash
narsor-web --open          # the default setup: the local daemon, browser opened
NARSOR_WEB_UPSTREAM=https://narsor.example.com \
NARSOR_TOKEN_COMMAND='my-oidc-token' narsor-web --open
```

The settings (`web_bind`, `web_upstream`, `web_token`) are in the shared
[configuration](configuration.md); `narsor-cli config show` lists them.
`narsor-web` accepts `--root` and `--config` like the other programs.

`web_bind` must be a loopback address (default `127.0.0.1:8484`).
`narsor-web` does not serve TLS yet, and a browser treats plain HTTP as a
secure context only on loopback. To reach the interface from other machines,
put a TLS proxy in front of a loopback `narsor-web`. A server in network mode
then lists the address of `narsor-web` in `trusted_proxies`; `narsor-web`
passes the proxy's `X-Forwarded-Proto` on in the `passthrough` mode.

## Tokens

- **`server` mode:** `narsor-web` holds the server's token (the local
  daemon's from its discovery file, or a remote server's from the token
  sources above, read again after a `401`). The browser gets a **session
  token** of `narsor-web`, new at each start: `narsor-web --open` puts it in
  the address, and it is in the file `web-session` (mode `0600`) of the
  state directory. The server's token never reaches the browser. If the
  local daemon restarts on another port with another token, `narsor-web`
  finds it again by itself.
- **`passthrough` mode:** the browser presents the server's own token (an
  OIDC access token of the installation owner; there is no OIDC sign-in in
  the browser yet). When it expires, the interface returns to the sign-in
  form.

The sign-in form says which token it needs (*Skąd wziąć token?*).

### Security

- In the `server` mode anyone who reaches `narsor-web` acts as the owner of
  the server. Therefore: loopback only; every API call must carry the
  session token (compared in constant time); a request whose `Host` is not
  `127.0.0.1`, `localhost` or `[::1]` with the right port, or whose
  `Origin` is another site, is refused with `403` (other sites and DNS
  rebinding).
- Only the routes the interface uses go through (`/control/v1/…`,
  `/settings/…`, `/auth/v1/whoami`, `POST /tasks`); anything else is a
  `404`, so `narsor-web` is no open relay.
- `--open` never puts the session token in a process argument: the browser
  opener gets a page in the state directory (`web-open.html`, mode `0600`)
  that sends the browser on to `/ui/#token=…`. A browser never sends the
  fragment to a server; the interface moves the token to the session storage
  of the tab and removes it from the address bar at once.
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
