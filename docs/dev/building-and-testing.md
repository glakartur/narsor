# Building and testing

Developer notes: prerequisites, building, testing, how harnesses are invoked, and known limitations. See also [CONTRIBUTING.md](../../CONTRIBUTING.md).

## Prerequisites

- **Rust** (2021 edition) with `cargo`.
- **`git` on `PATH` — a runtime prerequisite.** The narsor clones
  repositories by shelling out to the `git` command as a child process.
  `libgit2` was deliberately rejected, because it honours neither
  `~/.ssh/config` nor credential helpers nor `url.*.insteadOf` rules — which is
  exactly the configuration access to private repositories rests on. A missing
  `git` only shows up when a `git` workspace is provisioned, not at startup.
- **An agent harness on `PATH` — `claude` or `opencode` — a runtime
  prerequisite too**, and only for workflows that have an `agent` state. See
  [`AgentAction`](../user/agents.md#agentaction--running-an-ai-agent). A missing one shows up
  when such a state is entered, not at startup.
- A POSIX system. Read-only access to secondary repositories is enforced with
  file permission bits.

### Building and testing

```bash
cargo build --workspace --bins
cargo test --workspace
cargo test --workspace -- --include-ignored  # also process lifecycle tests
cargo build --workspace --bins --release
```

Ignored tests drive the real `claude` binary (they cost money and need
credentials), start `narsor-controller` processes, or need a live Kubernetes
cluster (the smoke test also requires its documented `NARSOR_K8S_SMOKE_*`
environment variables).

### Test layout

The integration tests of the `narsor` package are one test crate,
`tests/integration/main.rs`; each file next to it is a module and **must be
declared there with `mod`** to run (Cargo does not discover it otherwise).
Shared helpers are the `*_support` modules, used as `crate::<name>_support`.
All modules run concurrently in one process, so they must not use
process-global state (`set_var`, `set_current_dir`, fixed ports) and must use
temporary directories and ephemeral (`:0`) ports. Tests that spawn the built
binaries (`CARGO_BIN_EXE_*`) or `cargo` do not belong in this crate.

### Keeping `target/` small

`Cargo.toml` lowers debug info of external dependencies
(`line-tables-only`) and strips release binaries. Remove stale artifacts
periodically with `cargo sweep --time 14`.

The workspace build keeps `narsor`, `narsor-cli`, `narsor-server`,
`narsor-runner`, and `narsor-controller` together in `target/debug` (or
`target/release`), so the client can find the server and the server can find
the local controller and runner. In trusted mode the server starts
`narsor-controller local` as its child process; it does not start without that
binary next to it. To install all process binaries into the same directory:

```bash
cargo install --path . --bins
cargo install --path crates/narsor-server
cargo install --path crates/narsor-runner
cargo install --path crates/narsor-controller
```

The integration tests run against a real filesystem and spawn real child
processes — always in temporary directories, never inside the repository.

## How each harness is invoked

All three are run non-interactively, in the workspace, with
`ORCHESTRATOR_ACTION_ID` in the environment (D24), stdin closed, and both pipes
drained while the agent works.

```text
claude   --print --output-format json \
         [--add-dir <read_only_paths…>] [--model <model>] [--max-turns <n>] \
         [--append-system-prompt <text>] [--resume <session-id>] \
         [--allow-dangerously-skip-permissions] --permission-mode <mode> \
         <prompt>

opencode run [--model <provider/model#variant>] [--agent <agent>] \
         [--session <session-id>] [--fork] [--auto] --format json \
         <prompt>

codex exec [--model <model>] --sandbox <read-only|workspace-write> \
          --config approval_policy=never --json [resume <thread-id>] <prompt>
```

**Which login a harness uses.** A harness keeps its subscription login next
to its sessions — `codex` in `~/.codex/auth.json`, Claude Code in
`~/.claude/.credentials.json` — so which `HOME` the runner gives a task decides
whether the harness is logged in. A local task keeps the daemon's own `HOME` by
default (`local_home: host`), with the same logins as the console. `local_home:
private` gives every local task a fresh `HOME` on its own storage instead;
`local_codex_home` and `local_claude_config_dir` (absolute paths, exported as
`CODEX_HOME` and `CLAUDE_CONFIG_DIR`) then point the harnesses at a login
directory of your choice. `local_claude_profile_source` (an absolute path,
empty by default) names a Claude Code profile the runner merges into
Claude's configuration directory before every start; it copies nothing when
it is empty or the configuration directory itself, and it needs either
`local_claude_config_dir` or `local_home: private` (then
`$HOME/.claude` on the task storage). The runner reads these on every start — they are
the backend's settings, not part of a task — so switching them leaves a
running task's earlier sessions behind. Inherited `ANTHROPIC_API_KEY`,
`OPENAI_API_KEY` and `CODEX_API_KEY` are always dropped, so a harness never
switches to API billing behind the subscription's back; a task that should
use a key gets it as an `env` secret.

`--print` / `run` are what make the run a batch run; `json` output is what
carries the session id as well as the text. The flag that takes exactly one
value is always the last one before the prompt, so a prompt can never be read
as a flag's argument.

`workspace_access` is the portable access policy for the action: `read-write`
(the default) or `read-only`. It is translated to each harness's native flags,
so a workflow does not need to know which harness is selected. `parameters`
remain harness-specific for settings unrelated to that policy: `max_turns`,
`append_system_prompt` and `resume_session` for `claude-code`; `agent`, `fork`
and `resume_session` for `opencode`; `approval_policy` (only `never`) and
`resume_session` for `codex`. The former access knobs (`permission_mode`,
`dangerously_skip_permissions`, `auto_approve`, and `sandbox`) are rejected
to prevent a per-harness setting from overriding the portable policy. An
unknown value or a wrong type is an error
before anything is run — never a silent default.

## The JSON output contract

The agent is asked — the contract is appended to every prompt — to finish with
a single JSON object:

```json
{ "choice": "ready", "reason": "the plan covers every requirement", "variables": { "summary": "3 units" }, "artifacts": { "plan_doc": "plan.md" } }
```

`choice` must be one of the state's declared `choices`, and `reason` is one
line that explains it (asked for, but not enforced). `variables` reach the
context when the step declares them, named `artifacts` are registered, and
`additional_data` / `additional_artifacts` carry anything else — exactly as for
a script action. See workflow-schema.md §3.3.A for the full contract.

Agents do not reliably emit clean JSON, so the answer is read robustly: a bare
object, a fenced `json` code block, or an object trailing any amount of prose all
work, and the **last** object carrying a `choice` key wins. But **when no
usable label can be found, the action fails** — it is never guessed, and never
defaults to `success`. In an unattended pipeline a guessed label is a silently
wrong transition, which is worse than a failure an operator can see. The state's
`retry_policy` gives a hallucinating agent another attempt (D20); escalating to
a person afterwards is the state's own business.

## Installing a prebuilt Linux release

Prebuilt binaries for x86_64 Linux are installed with the release installer;
see [Linux installation and server operation](../operations/installation.md).

## Known limitations

Deliberately deferred, and verified against the code — each item needs a plan
of its own:

- **`WaitAction`** and the service that polls for external events: the action
  type parses, but no executor is registered, so entering such a state fails
  with "not available in this version".
- **Agent decisions** (`decision: { type: agent }`) — they return an explicit
  "not available" error instead of quietly behaving like a rule-based decision.
- **Containerisation of local tasks** — a task started by `narsor-controller
  local` runs as the user's account without a container, so isolation rests on
  filesystem permissions, which protects against a mistake and not against a
  malicious agent. Do not run untrusted workflows locally. (The Kubernetes
  backend runs tasks in pods; see [Remote execution backends](../operations/remote-backends.md).)
- **A web UI, metrics and distributed tracing** are not provided. (Server
  authentication through OIDC exists; see the operations guide.)
