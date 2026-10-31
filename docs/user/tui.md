# Terminal interface (TUI)

Reference for `narsor`, the full-screen terminal interface: panels, keys, forms and the conversation view. Operator commands live in [the CLI reference](cli.md).

## TUI (`narsor`)

The full-screen terminal interface: a `lazygit`-shaped console over the
control plane (`/control/v1`). There is one mode. The backend is the one the
configuration names: the local server of `--root` (started when none runs),
or a remote control plane with `--server` /
`NARSOR_SERVER` and a token (see [Remote tasks](remote.md#remote-tasks-and-backends)).
Run `narsor --server https://control.example` to open the TUI against a remote
server. Use `narsor-cli --server https://control.example ...` for CLI commands.

```text
narsor · local ● live             harness claude-code (konfiguracja) · sloty 2/3 · ? 1 · ! 1
╭▸ Zadania ────────────────╮╭ Zmiana API zamówień ─────────────────────────────────────────╮
│    ●  NAR-14 Edycja pars…││ Zmiana API zamówień · NAR-12                    implement v3 │
│       edit               ││ Jira NAR-12 ↗   PR !482 ↗   running · krok 3/5 · 14m         │
│──────────────────────────││ Przebieg  Rozmowa  Zdarzenia  Dyrygent  Info                 │
││ ? ●  Zmiana API zamówień││───────────────────│   Read src/api.rs                        │
││      approval           │││ ● review_api     │   Edit src/api.rs  +12 −3                │
│──────────────────────────││───────────────────│ │ assistant: Zmieniam sygnaturę…         │
│  ! ✗  Migracja bazy dany…││╭ ? Czeka na Ciebie: approval · 3m ──────────────────────────╮│
│       failed             │││ the answer panel                                           ││
│                          ││╰────────────────────────────────────────────────────────────╯│
│                          ││                                                              │
╰──────────────────────────╯╰──────────────────────────────────────────────────────────────╯
Enter szczegóły  Space następny czekający  . akcje  n nowe  h harness  ?           powiadom.
```

**The screen.** There is no frame around the screen: the first row is the
header — the connection and its state on the left; on the right the
installation's default agent harness (`harness claude-code (konfiguracja)`
when it comes from the configuration, `harness opencode` when the operator
chose it, `harness ?` before the first read), the slots and the counters of
what waits. The last row is the shortcut bar. On its right it shows a
notification while one is visible, otherwise the subscription limits the
runners last reported for each harness (`claude 5h 1% ↻4h10m · 7d 90% ↻2d3h`:
window, use, time to reset; a warning in the attention colour, a rejection in
the problem colour, with `!` before the window when colours are off). The TUI
reads them every 15 s; when they do not fit, the last harness goes first, then
the end is cut with `…`, and the shortcuts give way to them. Every panel between them has a
frame of its own with rounded corners: in the focus colour (cyan, with a bold
title and a `▸` before it) on the panel that has the keys, grey on the others —
so the active panel reads without colour too. The details frame is named after
the task and keeps the focus look while the keys are in the answer panel. New task, history and
repositories use the same frames.

**The list** is the queue: tasks stand in the order of the scheduler (`rank`),
are never grouped or sorted by state, and change place only by an explicit
action (`K` / `J` / `m`, the TUI's `narsor-cli task move`). A row takes two lines.
The first holds the attention mark (`?` a question or an approval, `!` a
problem, a failed step or a failed provisioning), the state icon (`●` running,
`◷` queued, `⟳` provisioning, `‖` paused, `✗` failed or interrupted, `✓` done,
`↻` being sent) and the title from the workflow's `display` (the task id when
there is none) on the whole width of the list. The second, under the title,
is the label: the current step, `#n` in the queue, `offline` when no
controller holds the backend, `nie wysłano`, or the place preview `↕ n/m`
while moving. Every row is two lines even with no label and the rows stack
directly, so the list scrolls by whole tasks. The selection is a bracket,
never a fill: a thin rule across the whole width of the list in a row of its
own directly above the task, another directly below it and a bar beside both
its lines, so the row keeps its own colours. Only the selected task has the
rules, so moving the selection shifts its neighbours by a row. The bracket is
always thin: cyan where the keys are, grey elsewhere (bold and dim without
colour). The history, the steps on *Przebieg* and every list of choices (the
menu, the answer panel, the new task, the repositories) mark their selection
the same way; a conversation and *Zdarzenia* keep only the bar. `z` hides finished tasks as
a filter; `a` archives one. The selection follows the task, not the row,
through every refresh.

Each task uses three lines: its ticket (or an `id:` prefix and the first eight
id characters), title, then state, time in that state and the existing short
label. Activity time advances on redraw; tasks without a known start time and
finished tasks show no age. `/` filters by ticket, title or full id without
changing server order; state changes and refreshes likewise keep rows in place,
and only an explicit queue move reorders them. `Esc` cancels filter editing.

**The details** show the header (title and links from `display`, lifecycle,
step, duration) and five tabs: *Przebieg* (the steps and the selected one),
*Rozmowa* (the agent's conversation), *Zdarzenia*, *Dyrygent* (the conductor's
chat and decisions; its status line names the harness the conductor runs on)
and *Info* (the sealed inputs, the observations with their age, the runner and
the controller). `[` / `]` switch the tabs. *Przebieg* has two levels of
focus: the steps, where the details open, and the conversation of the selected
step. `Enter` / `→` on a step that has a conversation to show go into it
(a running step from its first entry on), and there `j` / `k` pick an entry
and the keys of *Rozmowa* work; `Esc` / `←` go back to the same step. While
the keys are in a step's conversation the timeline does not move by itself,
even when following the current step; going back catches up. The history's
details work the same way, without following. A task that has not started says so and where it stands in
the queue. When a running task cannot follow the default harness — its
backend does not run it, or the binary is not on the runner's `PATH` — one
line under the header says so (`opencode niedostępny tutaj — zostaje
claude-code`) and *Info* gives the full reason in the runner's section. It is
information, not a problem: no list mark, no attention count.

**The default harness** (`h` on the list, in the details, the history and the
repositories — never in a text field, a menu or over another overlay) opens a
list: first `domyślny z konfiguracji (<name>)`, which removes the operator's
choice, then the harnesses the daemon supports; the one in force is marked
`●`. `Enter` sends the choice and the header shows `harness <new>…` until the
answer; a refusal comes back as a notification with its cause, the old value
kept. The choice is the installation's, not the task's: new tasks take it, and
tasks already running take it from their next agent action and the
conductor's next invocation (see [`AgentAction`](agents.md#agentaction--running-an-ai-agent)).

**What waits for you** is one panel at the bottom of the details, whatever the
kind of waiting: a question or a `human` gate, an approval, a problem, a failed
step (with the conductor's diagnosis and proposals, or the raw error), a
proposal of the conductor's, a failed provisioning. Every choice is a list with
a description of its effect next to it; `Tab` moves between the choices and
their fields, `Enter` sends, `Esc` folds the panel and keeps what was typed.
Action 0, `Napisz do agenta`, is the first choice of every panel: type a
message and send it. The panel keeps the text when the send fails and empties
the field when it succeeds.

When the server offers forms for the task (`TaskSummary.panel`), the panel
shows one form at a time — an answer, the free conversation with the agent, an
action of the workflow, a permission, the entry limit — and `Esc` steps through
them, also from a field; the first line says which one (`1/2 · Esc przełącza`).
The server says which form opens first and when there is none (the agent is at
work). It also says what each form of a wait asks — its entries, their fields,
how the chosen entry becomes the answer and what the notification says after it
(`narsor_protocol::panel`); the panel only shows that and sends the answer
back. Each form keeps its own draft. The conversation is a text box that grows
to half the height of the details; `Ctrl+J` makes a new line, `Enter` asks
before it sends. Such a panel is not folded by `Esc`; `Tab` leaves it.
`Space` on the list jumps to the next task that waits, `[` / `]` move between
several waitings of one task. The header counts them (`? 1 · ! 1`).

**The action menu** (`.`) lists everything that can be done with the selected
task, each with its shortcut, and greys out what cannot be done with the
reason. What can be done comes from the server (it checks the same rule when
the command arrives); a server that does not say shows "niedostępne: brak
danych". An action marked `!` asks `y` / `n` first.

| Key | Action | The CLI it stands for |
|---|---|---|
| `K` / `J` / `m` | move in the queue (a place preview, `Enter` confirms) | `narsor-cli task move` |
| `p` | pause or resume | `narsor-cli task pause` / `resume` |
| `x` | interrupt the running step (`!`) | `narsor-cli task interrupt` |
| `t` | manual transition; with a reason, an override where the state allows it | `narsor-cli task transition [--override --reason]` |
| `s` | start again an interrupted task or a failed provisioning | `narsor-cli task start` |
| `a` | archive (`!`) | `narsor-cli task archive` |
| `o` | open a link of the task (`display.links`) in the browser | — |
| `c` | action 0, `Napisz do agenta` (first in the menu), in one step: the server picks what it does by the way the task is stopped — a question: a message in the same session; an approval or a permission: a refusal with your note, never an approval; a pause: the note goes to the agent and the task resumes. A text that was not sent stays as a draft of the task | — |
| `c` (*Dyrygent*) | write to the conductor | `narsor-cli task conductor chat` |
| `u` (*Dyrygent*) | undo a decision of the conductor (`!`) | `narsor-cli task conductor undo` |
| the panel | confirm / reject a proposal, restart a failed step with a note | `narsor-cli task conductor confirm` / `reject`, `narsor-cli task restart-step` |
| `n` | new task (a catalogue workflow, or the path of a definition) | `narsor-cli task create <workflow>`, `narsor-cli task create --file <path>`; the catalogue is `narsor-cli task workflows`, the check of a file `narsor-cli task validate <path>` |
| `i` / the panel | answer what waits: fields, `artifacts`, `variables`, label, comment | `narsor-cli task input <task> --field … --artifact … --variable … [--waiting-id ID]` |
| `Space` | the next task that waits | `narsor-cli task show` ends with `Next       waiting: <task>` |
| `H` | history: archived tasks, and executions from before the scheduler (`w` recovers a failed one) | `narsor-cli task list --archived`; `narsor-cli task history list` / `show` / `events` / `steps` / `conversation` / `conductor` / `recover` |
| `R` | repositories: `a` registers, `d` removes (`!`) | `narsor-cli task repo list` / `add` / `remove` (the same registry on any server; `narsor-cli repo …` is the local one) |
| `h` | the installation's default agent harness: choose one, or go back to the configured one | `narsor-cli harness default set <harness>` / `reset` (`narsor-cli harness default` shows it) |
| the header | what the connection offers | `narsor-cli task capabilities` |

The CLI of the remote-aware commands has a machine form too: `narsor-cli task list
--json` and `narsor-cli task show <task> --json` print the task summaries of the
control plane as JSON (text an agent or a runner wrote is cleaned of what a
terminal would act on). A workflow file, a file validation and the old
executions of the history belong to the local daemon: any other server refuses
them with the reason `narsor-cli task capabilities` gives, and a repository is
registered there by a remote git URL only.

`n` opens the catalogue of workflows (built-in, user and root ones, with the
source of each and what a copy overrides) with the inputs of the highlighted
one as the form: the declared `type` decides what a value is (`boolean` and
`repository` are choices, `integer` is checked) and a missing required input
is reported next to the field. The last entry takes the path of a definition
you are writing — validated first, every violation listed, as
`narsor-cli validate` does — which only a local server accepts. A new task is a
row at the end of the list (`↻ wysyłanie…`) until the server lists it; one
whose answer was lost is sent again with the same id, and `r` sends one that
did not arrive.

**Connection.** The interface opens even when the server cannot be reached
(`łączę z … ◌`) and goes on trying. The header shows the state of the stream
(`● live`, `◌ ponawiam (12s)`, `✖ brak połączenia`); without a connection the
data is greyed and marked with its age (`stan sprzed 40s`), actions that change
state are blocked with the reason, and drafts and forms stay. The stream always
resumes from the last event applied. Events of a task patch its row at once, the
rest is read again after a short delay, and every `tui_refresh_interval` (10 s
by default, `none` turns it off; see [Configuration](configuration.md#configuration)). A runner
report that changes no state, question or attention does not start a read, so the
current step of a task refreshes every `tui_refresh_interval`. A read
never changes the selection, the focus, the tab, the cursors, the following of
the current step or a draft.

**Keys.** One key map serves the handling, the shortcut bar and the help under
`?`, and one key means one thing in a context.

| Where | Keys |
|---|---|
| Everywhere | `?` help, `q` quit (asks when a draft would be lost) |
| List | `j` / `k` (`↓` / `↑`), `g` / `G`, `/` filter, `Space` next waiting, `Enter` details, `.` menu, `n` `z` `H` `R`, `K` `J` `m`, `p` `x` `t` `s` `a` `o` `c` `r`, `h` default harness |
| Details | `[` / `]` tabs, `Tab` content ↔ panel, `.` menu, `h` default harness (the history's details: `[` / `]`, `w` recover, `h`); then by where the keys are: |
| — *Przebieg*, the steps | `Esc` list, `j` / `k` (`↓` / `↑`) `g` / `G` pick a step, `Enter` / `→` the step's conversation, `f` follow the current step |
| — *Przebieg*, a step's conversation | `Esc` / `←` back to the steps, `j` / `k` `PgUp` / `PgDn` `g` / `G` pick and scroll, `Enter` / `Space` expand an entry, `v` raw entry, `Ctrl+V` raw all |
| — the other tabs | `Esc` list, `j` / `k` `PgUp` / `PgDn` `g` / `G` scroll, `Enter` / `Space` expand an entry, `v` raw entry, `Ctrl+V` raw all, `f` to the end and follow |
| Panel | `Tab` actions ↔ fields, `Enter` send, `Ctrl+J` new line (also `Shift+Enter` / `Alt+Enter` where the terminal reports modifiers), `Esc` fold (with the server's forms: next form), `1`–`9` choose, `[` / `]` previous / next waiting (not with the server's forms) |
| Menu | `j` / `k`, `1`–`9`, `Enter`, `Esc` and the shortcuts above |
| Confirmation | `y` confirm, `n` / `Esc` take it back |
| Forms | `Tab` / `Shift+Tab`, `←` / `→`, `Enter` send, `Ctrl+J` new line, `Esc` back (from a new-task field to the list, then close; what was typed stays) |

Colour means something: yellow what needs you, red what is wrong or cannot be
undone, green what happens now, grey what is done or unavailable, cyan focus.
Only the 16 ANSI colours are used, so the interface follows the terminal's
theme; with `NO_COLOR` set it uses bold, dim and reverse only, and every state
is still told by its icon and its word.

**Logging.** `tui` takes over the alternate screen, so a log line on stderr
would be written over the panels. With `RUST_LOG` set, the interface's logs are
appended to `{root}/narsor.log` instead; with `RUST_LOG` unset no
subscriber is installed at all and the records are dropped. Either way nothing
reaches the terminal while the interface is up. Every other subcommand keeps
logging to stderr.
