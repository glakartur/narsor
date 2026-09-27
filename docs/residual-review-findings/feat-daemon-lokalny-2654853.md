# Residual code review findings — `feat/daemon-lokalny`

Plan: `docs/plans/2026-09-27-001-feat-daemon-lokalny-plan.md`
Review run: Tier 2, 8 parallel personas (correctness, testing, maintainability,
project-standards, security, adversarial, api-contract, reliability, agent-native)
against `4fe76f4761e8a6728089fb383f2a343cba4d7a85..2654853` (U1–U8 plus fixes).

Four confirmed bugs from this review were fixed with regression tests and are in
the branch's own commit history (see `9b6672d` and `2654853`):
stream_all() ending on any execution's terminal event, the daemon's shutdown
ordering race, the unreapable `daemon.lock` on a dead spawner, and the
`EngineError`→500 mapping gap for caller-mistake preconditions.

The findings below were accepted as-is rather than fixed, per the user's
"Accept and proceed" decision at the Residual Work Gate. None block this
branch from shipping.

## P1 — discovery.rs: a slow (not dead) daemon can still get double-spawned

`ensure_daemon_running`'s `DISCOVERY_TIMEOUT` (15s) frees `daemon.lock` even
when the daemon it was waiting on is merely slow to start, not dead. A caller
that gives up and retries after that timeout could spawn a second, genuinely
independent daemon against the same state root if the first one eventually
succeeds anyway. Closing this fully would mean holding an flock-style lock for
the daemon's entire lifetime (released only on daemon exit), not just during
spawn — a real architectural change, not a local fix. In every observed run
a daemon starts in well under a second, so this is a narrow edge case for
the plan's stated single-local-operator scope.

*Reviewer: adversarial. Confidence 62.*

## P1 — SSE `Lagged` on the shared broadcast buffer could drop a terminal event

`Engine::subscribe()`'s single 1024-capacity broadcast channel is shared by
every execution. A sufficiently overwhelmed consumer of `stream_execution`
could have an unrelated execution's traffic push its own terminal event out
of the ring buffer before it's read, in principle stalling a caller that
depends solely on that stream ending. In practice no current caller depends
on it exclusively — `src/cli/commands.rs::wait_until_stopped` and the TUI's
`refresh()`-after-every-mutation both poll `GET /executions` as an
authoritative backstop specifically because of a related, already-documented
gap (SSE has no replay). Fixing the shared-buffer root cause would mean
per-execution channels or a much larger buffer — a bigger design change.

*Reviewer: adversarial. Confidence 55.*

## P1 (pre-existing) — `post_transition` has a TOCTOU race

`POST /executions/:id/transition` checks `engine.is_active()` once, then
performs several unlocked event-log writes and a `save_execution`. A
concurrent `resume`/`deliver_human_input` claiming the execution between the
check and the writes could race this handler's own writes. This is inherited
verbatim from the TUI's original `write_transition` (which had the identical
single-check shape) — not introduced by this diff — but the exposure widened:
the TUI's menu never offered this concurrently with itself in a way that hit
the race in practice, whereas this endpoint is now reachable by any
bearer-token holder. A real fix would have the handler acquire the same
active-run claim `try_begin_resume`/`try_begin_deliver_human_input` use for
the whole read-modify-write, not just a snapshot check.

*Reviewer: correctness. Confidence 55. `pre_existing: true`.*

## P2 — permission-based `daemon.json` unlink across OS users sharing a state root

`ensure_daemon_running` treats any `DaemonInfo::read` error (not just
"file not found") as "no daemon, clean it up," and unconditionally unlinks
the discovery file. On a state root writable by more than one OS user, an
unrelated user only needs directory write permission (not the 0600 file's
read permission) to remove another user's live discovery file and spawn a
second daemon while the first is still running and now undiscoverable. This
sits outside the trust boundary the design already accepts (D52/D53's own
rationale: loopback + a bearer token defend against *other local users*
guessing the token, not against a state root deliberately shared with write
access across accounts — an unusual deployment for a single-operator tool).

*Reviewer: adversarial. Confidence 48.*

## P2 — duplicated `ClientError`→message and status-name mapping (CLI vs TUI)

`src/cli/commands.rs::client_error`/`status_name` and
`src/tui/app.rs::client_error_message`/`status_name` are near-identical
matches over `ClientError`/`ExecutionStatus`, each unit's own doc comments
acknowledging the other as its "counterpart" rather than a shared helper.
The two `Conflict`-case sentences have already drifted slightly. Worth
centralizing before a third REST client, or a new `ClientError` variant,
is added — but it's a refactor touching both files' logic, not a
same-behavior mechanical extraction.

*Reviewer: maintainability. Confidence 75.*

## P3 — dead `WorkPaths` field on the TUI's `App`

`src/tui/app.rs` keeps a `work: WorkPaths` field behind `#[allow(dead_code)]`
with a comment saying nothing currently reads it, justified as "constructor
symmetry" and for an unnamed future feature. Zero consumers today.

*Reviewer: maintainability. Confidence 60.*

## Also noted, not a finding requiring action

- **Security (informational):** `POST /tasks`'s `workflow` field accepts any
  existing file path, not just catalog-relative references — this mirrors
  `narsor run <path>`'s existing CLI behavior verbatim and doesn't grant a
  token holder anything beyond what they already have as the local OS user.
  Worth reconsidering only if a daemon is ever shared across trust levels.
- **Testing gap:** no concurrent/409 test for `POST /executions/:id/input`
  analogous to the existing one for `/resume` (same underlying
  `claim_active_run` code, lower-priority asymmetry in coverage, not in
  behavior).
