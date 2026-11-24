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
credentials), start `narsor-controller` processes, start the stub runner
processes of the local backend contract suites (`backend_contract::local`,
`backend_contract::local_docker`), or need a live Kubernetes cluster (the
smoke test also requires its documented `NARSOR_K8S_SMOKE_*` environment
variables). The default `cargo test` and CI do not run them; run `cargo test
-p narsor --test integration backend_contract -- --include-ignored` after a
change to the local backend.

The Docker work mode of the local backend has its contract target in
`tests/integration/backend_contract_support/local_docker.rs`. It needs **no
Docker daemon**: the adapter talks to `FakeDocker`
(`crates/narsor-controller/src/local/container/docker_fake.rs`), which builds
images in memory and, for a started container, runs the container's program — the
stub runner mounted at the runner's path — as a host process with the
container's arguments and only its environment. Killing that process is
`docker kill`; `remove_container` is `docker rm -f`. So the whole suite, and
`backend_contract::a_container_removed_from_outside_is_resumed_on_the_same_image`,
drive the real container path of the adapter (preflight, background build,
per-task record, `docker run` request, observe and remove by name). They
cannot prove that an image builds or that the runner starts in it, nor the
mounts, the user and the hardening of a real `docker run`: unit tests check
the `docker` argument lists, and a change there is checked by hand on a real
daemon (a task with a small `task_image` and a `script` step; see
`docs/user/local-docker.md`). No committed test calls the `docker` CLI.

### The browser interface (`crates/narsor-web`)

`crates/narsor-web` builds the `narsor-web` binary: the HTTP server of the
browser interface and a proxy of the owner API to a `narsor-server`
(`src/gateway.rs`, settings and their rules in `src/settings.rs`). The files
of the interface are in `crates/narsor-web/ui/` and are compiled into the
binary (`include_dir`); `build.rs` rebuilds the crate when a file there
changes. `narsor-server` itself serves no interface. The application is plain ES modules with one
vendored library (`ui/vendor/preact-htm.js`, licences in
`crates/narsor-web/VENDOR-LICENSES.md`), so neither the build nor a release
needs Node.

The rules of the interface (state icons, panel deliveries, the new-task form,
texts) are pure modules with tests for Node 22 or newer:

```bash
node --test crates/narsor-web/ui-tests/*.mjs
```

These tests are not part of `cargo test`. In a view, never use `innerHTML`
or `dangerouslySetInnerHTML`, and never add inline scripts or styles: the
content security policy refuses them.

### The version of a build

Every binary reports `narsor_protocol::VERSION` (`--version`). It is the
version of the crate (`Cargo.toml`), unless the build sets
`NARSOR_BUILD_VERSION`: the release workflow sets it from the release tag
without the leading `v` (`v1.2.3` → `1.2.3`), and `dist/package-linux.sh`
checks that `narsor-cli --version` reports it and writes it into
`manifest.json`. To build a binary with a version locally:

```bash
NARSOR_BUILD_VERSION=1.2.3 cargo build --workspace --bins --release
```

(`NARSOR_VERSION` is a different variable: the release tag `dist/install.sh`
downloads.)

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
`narsor-runner`, `narsor-controller` and `narsor-observer-worker` together in
`target/debug` (or `target/release`), so the client can find the server and
the server can find the local controller, the runner and the observer worker. In trusted mode the server starts
`narsor-controller local` as its child process; it does not start without that
binary next to it. To install all process binaries into the same directory:

```bash
cargo install --path . --bins
cargo install --path crates/narsor-server
cargo install --path crates/narsor-runner
cargo install --path crates/narsor-controller
cargo install --path crates/narsor-observer-worker
```

The integration tests run against a real filesystem and spawn real child
processes — always in temporary directories, never inside the repository.

## How each harness is invoked

Every caller — the agent step, the profile step, the conductor — runs a
harness the same way: it builds one `AgentRequest` (prompt, context files,
model tier, effort, session to resume, working directory, workspace access,
directories, parameters, answer schema and shape, time limit, extra args)
and calls `HarnessAdapter::run(request, observers)`. The harness translates
the request, starts its CLI or SDK, hands over the prompt, makes the context
files readable, and parses the output; the observers receive the conversation
lines, the usage limits, and the model and effort the harness resolved
(`AgentSessionStarted` is emitted from them). No caller builds or reads a
command line.

The CLI harnesses share the process mechanics in
`crates/narsor-exec/src/engine/actions/harness/cli.rs`: each runs
non-interactively in its working directory, with `ORCHESTRATOR_ACTION_ID` in
the environment for an agent step (D24, not for the conductor), as the leader
of its own process group, with both pipes drained while the agent works.

```text
claude   --print --verbose --permission-prompts none \
         [--add-dir <read_only_paths…> <dirs outside cwd…> <context dir>] \
         [--allowedTools <rule…>] --disallowedTools ScheduleWakeup CronCreate Monitor \
         [--model <model>] [--effort <effort>] [--max-turns <n>] \
         [--append-system-prompt <text>] [--resume <session-id>] \
         [--json-schema <schema>] --output-format stream-json \
         [--permission-mode <mode>] [<extra args…>] [<prompt>]

opencode run [--model <provider/model#variant>] [--agent <agent>] \
         [--session <session-id>] [--fork] [--auto] --format json \
         [<extra args…>] [<prompt>]

codex exec [--model <model>] [-c model_reasoning_effort="<level>"] \
          [--sandbox <read-only|workspace-write>] [--add-dir <writable dir…>] \
          --config approval_policy=never --json [resume <thread-id>] \
          [<extra args…>] <prompt | ->
```

**Prompt transport.** A prompt up to 96 KiB is the last argument. A longer
one goes on stdin, because Linux refuses a single argument over 128 KiB
(E2BIG, NARSOR-1124-001): `claude --print` and `opencode run` read it when no
prompt argument is given, `codex exec` when the prompt argument is `-`. The
codex and opencode behaviour is recorded in `tests/fixtures/codex/` and
`tests/fixtures/opencode/`. Without a long prompt, stdin is closed.

**Extra args** from `agents.<harness>.extra_args` replace the same flags the
harness chose and go after its own flags, right before the prompt.

**Context files.** A request may carry context files and a directory for them
(the conductor uses this; agent steps do not yet). The harness empties the
directory, writes the files, and appends a `## Context files` block to the
prompt with each file's absolute path, size and one-line description. Read
access: `claude` gets the directory as one more `--add-dir`; codex's sandboxes
read the whole disk (checked with `codex sandbox`), and `--add-dir` would make
the directory writable, so codex gets no flag; opencode and the Copilot SDK
read outside their working directory. A future harness that cannot reach
the directory must inline the files' content into the prompt instead (R6).

`github-copilot` is not spawned this way. Its `run` calls the GitHub Copilot
SDK (`github-copilot-sdk`) in-process; the SDK starts the operator's Copilot
CLI as `copilot --server --stdio` and talks JSON-RPC to it, and the prompt is
one SDK message, with no size limit of the command line. The runner finds the
CLI itself: `COPILOT_CLI_PATH` when it is set, else `copilot` on `PATH` (the
program the runner's PATH check looks for). The SDK is behind the
`copilot-sdk` feature of `narsor-exec`, and only `narsor-runner` enables it.
The SDK's default feature `bundled-cli` is off, because it embeds the whole
Copilot CLI runtime (about 200 MB) in every binary that links the SDK.
**`github-copilot` does not enforce `workspace_access`:** the SDK session
approves every permission, so a read-only step can write. The prompt names the
read-only paths, and the conversation records that read-only access is not
enforced; the fix is a separate change with its own security review.

**Which login a harness uses.** A harness keeps its subscription login next
to its sessions — `codex` in `~/.codex/auth.json`, Claude Code in
`~/.claude/.credentials.json` — so which `HOME` the runner gives a task decides
whether the harness is logged in. A local task keeps the daemon's own `HOME` by
default (`controller.local.home: host`), with the same logins as the
console. `controller.local.home: private` gives every local task a fresh
`HOME` on its own storage instead; `controller.local.codex_home` and
`controller.local.claude_config_dir` (absolute paths, exported as
`CODEX_HOME` and `CLAUDE_CONFIG_DIR`) then point the harnesses at a login
directory of your choice. `controller.local.claude_profile_source` (an
absolute path, empty by default) names a Claude Code profile the runner
merges into Claude's configuration directory before every start; it copies
nothing when it is empty or the configuration directory itself, and it
needs either `controller.local.claude_config_dir` or
`controller.local.home: private` (then `$HOME/.claude` on the task
storage). These settings belong to the `controller` section: the local
controller reads them, not the server. The runner reads these on every start — they are
the backend's settings, not part of a task — so switching them leaves a
running task's earlier sessions behind. Inherited `ANTHROPIC_API_KEY`,
`OPENAI_API_KEY` and `CODEX_API_KEY` are always dropped, so a harness never
switches to API billing behind the subscription's back; a task that should
use a key gets it as an `env` secret.

`--print` / `run` / `exec` are what make the run a batch run; the JSON stream
output is what carries the session id as well as the text. The flag that takes
exactly one value is always the last one of the harness's own before the
prompt, so a prompt can never be read as a flag's argument.

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

- **Agent decisions** (`decision: { type: agent }`) — they return an explicit
  "not available" error instead of quietly behaving like a rule-based decision.
- **Containerisation of local tasks** — a task started by `narsor-controller
  local` runs as the user's account without a container, so isolation rests on
  filesystem permissions, which protects against a mistake and not against a
  malicious agent. Do not run untrusted workflows locally. (The Kubernetes
  backend runs tasks in pods; see [Remote execution backends](../operations/remote-backends.md).)
- **A web UI, metrics and distributed tracing** are not provided. (Server
  authentication through OIDC exists; see the operations guide.)
