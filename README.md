# Narsor

**Narsor is a software factory built on AI agents and a flexibly defined
workflow.** It is not just a workflow processor: the goal is to turn an idea
or a task into delivered software by orchestrating agents (Claude Code, Codex,
OpenCode, GitHub Copilot) and people along a process you define yourself.

The process is described in YAML — a graph of states, actions, decisions and
transitions — so the same factory can be shaped to different teams, projects
and ways of working: from a single agent fixing a bug to a multi-stage line
with analysis, implementation, review and human approval. Agents do the work,
people stay in control where it matters, and every run is traceable and
recoverable (state and events are persisted, so runs can be resumed and
audited).

## Features

- **YAML workflows** — states, actions, decisions and transitions, validated
  before they run; a built-in `template` workflow ships with the binary.
- **AI agent steps** — run Claude Code, Codex, OpenCode or GitHub Copilot as the
  harness of a state, with per-state permissions and read-only repositories.
- **Human steps and approvals** — a workflow can pause and ask a person for
  input, an approval or a decision.
- **CLI and full-screen TUI** — `narsor-cli` for scripting, `narsor` for an
  interactive view of tasks, events and conversations.
- **Resume and recover** — interrupted or failed executions can be resumed or
  recovered, and every event is kept for audit.
- **Agent conversation** — read and continue the conversation with an agent at
  any step that is waiting.
- **Conductor** — a per-execution assistant that proposes actions you confirm
  or reject.
- **Repositories and workspaces** — a registry of repositories; each task gets
  its own git or directory workspace.
- **Remote backends and a server** — run tasks on registered backends
  (Kubernetes, or a local one) controlled through `narsor-server` with OIDC
  authentication.
- **Harness usage limits** — subscription limits of the agent harnesses are
  tracked and shown.

## Install

Requirements: x86_64 Linux with a compatible glibc (musl-only systems such as
Alpine are not supported), `git` on `PATH`, and — for workflows with agent
steps — an agent harness (for example `claude`, `codex` or `opencode`) on
`PATH`. Rust is not needed.

```bash
curl -fsSL https://github.com/glakartur/narsor/releases/download/v0.4.0/install.sh | bash
```

The installer offers three roles: **local** (everything on one machine),
**frontend** (TUI and CLI only) and **server** (server, controller and runner).
It asks where to put the binaries and the data (default `~/.narsor`) and never
overwrites an existing configuration file.

Unattended install, for example in a container build:

```bash
curl -fsSL https://github.com/glakartur/narsor/releases/download/v0.4.0/install.sh |
  bash -s -- --role frontend --non-interactive \
    --bin-dir /usr/local/bin --data-dir /var/lib/narsor
```

Uninstall (add `--purge` to delete configuration and data as well):

```bash
curl -fsSL https://github.com/glakartur/narsor/releases/download/v0.4.0/install.sh |
  bash -s -- --uninstall
```

Server setup, flags and the download checksum are described in
[Linux installation and server operation](docs/operations/installation.md).
To build from source, see [CONTRIBUTING.md](CONTRIBUTING.md).

## Quick start

```bash
narsor-cli validate tests/fixtures/cli/greet.yaml   # check a definition
narsor-cli run tests/fixtures/cli/greet.yaml \
    --input "greeting=Good morning" \
    --input "script_path=$PWD/tests/fixtures/cli/announce.sh"
narsor-cli list                                     # then: narsor-cli show <EXECUTION_ID>
narsor                                              # or open the full-screen interface
```

The example files are in this repository; `narsor-cli run template --input
example_text="Hello"` runs the built-in workflow from anywhere.

## Documentation

The full index is in [`docs/README.md`](docs/README.md).

- **Using Narsor** — [CLI](docs/user/cli.md), [TUI](docs/user/tui.md),
  [workflows](docs/user/workflows.md), [AI agents](docs/user/agents.md),
  [conductor](docs/user/conductor.md), [remote tasks](docs/user/remote.md),
  [configuration](docs/user/configuration.md).
- **Operating** — [installation and server](docs/operations/installation.md),
  [remote backends](docs/operations/remote-backends.md).
- **Developing** — [CONTRIBUTING.md](CONTRIBUTING.md),
  [building and testing](docs/dev/building-and-testing.md),
  [architecture decisions](docs/dev/architecture.md),
  [domain model](docs/dev/domain-model.md),
  [workflow schema](docs/dev/workflow-schema.md).

## License

See [`LICENSE`](LICENSE).
