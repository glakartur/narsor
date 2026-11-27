<p align="center">
  <img src="docs/assets/narsor-logo.svg" alt="Narsor" width="232">
</p>

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

### What makes Narsor different

- **The conductor: an AI supervisor for each task** — every task has its own
  assistant that watches the run and acts on its own when it is sure. It
  chooses the output of an agent step that gave no usable answer, restarts a
  failed step when that is safe, decides `decision` steps marked
  `by: conductor`, and answers questions of agents (`autopilot: on` or
  `best-effort`). When it is not sure, it gives the decision to a person,
  with its analysis and a proposal. Each decision is recorded with its
  confidence and rationale. You can also talk to it about the task in a chat.
- **Model and effort chosen per step** — a step asks for a model tier
  (`small` … `max`) and an effort (`low` … `max`); each harness maps the tier
  to its own model. A `profile` step lets an agent decide how strong a model
  the next steps need, and repeated failures of a step raise its tier and
  effort up to a `ceiling`. Simple steps stay cheap, and hard steps get a
  stronger model.
- **Multi-repository tasks** — one workflow works on several repositories.
  Each step says which repositories it may change, which it may only read,
  and where it starts. You choose the repositories and branches when you
  create the task.
- **Automations and queues** — shell scripts in the catalogue connect Narsor
  to Jira, GitHub, GitLab or any other source. They create tasks from new
  items (for example, new Jira issues), and they move a waiting workflow on
  when something happens outside (for example, when the reviewer of a merge
  request is done).
- **One system, from a laptop to a cluster** — the same workflows run:
  - fully on one machine, with each task as a local process or in its own
    Docker container;
  - on a server, with task runners on Kubernetes;
  - with the TUI, CLI or browser interface on your machine, connected to a
    remote server that authenticates users with OIDC.
- **Cost under control** — tokens and cost are counted for each step and
  each task, and the subscription limits of Claude Code and GitHub Copilot
  are shown in the interface.

### Also included

- **Four agent harnesses** — Claude Code, Codex, OpenCode and GitHub
  Copilot. A workflow can name one per step, or use the installation's
  default.
- **YAML workflows** — agent, script, human and decision steps connected by
  labeled transitions, validated before they run.
- **People in the loop** — a workflow stops for an approval or a decision,
  an agent can ask a question, and you can write to the agent of a waiting
  or failed step.
- **Full history** — the state and every event of a task are persisted, so a
  task can be resumed after a restart and audited later.
- **CLI, TUI and browser interface** — `narsor-cli` for scripts, `narsor`
  in the terminal, `narsor-web` in a browser.

## Install

Requirements: x86_64 Linux with a compatible glibc (musl-only systems such as
Alpine are not supported), `git` on `PATH`, and — for workflows with agent
steps — an agent harness (for example `claude`, `codex` or `opencode`) on
`PATH`. Rust is not needed.

```bash
curl -fsSL https://github.com/glakartur/narsor/releases/download/v0.35.0/install.sh | bash
```

Add `--progress` (`... | bash -s -- --progress`) to show a progress bar while
the archive downloads.

The installer offers three roles: **local** (everything on one machine),
**frontend** (TUI and CLI only) and **server** (server, controller and runner).
It asks where to put the binaries and the data (default `~/.narsor`) and never
overwrites an existing configuration file.

Unattended install, for example in a container build:

```bash
curl -fsSL https://github.com/glakartur/narsor/releases/download/v0.35.0/install.sh |
  bash -s -- --role frontend --non-interactive \
    --bin-dir /usr/local/bin --data-dir /var/lib/narsor
```

Uninstall (add `--purge` to delete configuration and data as well):

```bash
curl -fsSL https://github.com/glakartur/narsor/releases/download/v0.35.0/install.sh |
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
example_text="Hello"` runs the built-in workflow from anywhere. An example
catalogue of automations and queues (a review label on pull and merge
requests, new Jira issues as tasks) is in [`examples/catalog/`](examples/catalog/);
see [automations and queues](docs/user/observers.md).

## Documentation

The full index is in [`docs/README.md`](docs/README.md).

- **Using Narsor** — [CLI](docs/user/cli.md), [TUI](docs/user/tui.md),
  [browser interface](docs/user/web.md),
  [workflows](docs/user/workflows.md), [AI agents](docs/user/agents.md),
  [conductor](docs/user/conductor.md), [remote tasks](docs/user/remote.md),
  [automations and queues](docs/user/observers.md), [configuration](docs/user/configuration.md).
- **Operating** — [installation and server](docs/operations/installation.md),
  [remote backends](docs/operations/remote-backends.md),
  [updates](docs/operations/updates.md).
- **Developing** — [CONTRIBUTING.md](CONTRIBUTING.md),
  [building and testing](docs/dev/building-and-testing.md),
  [architecture decisions](docs/dev/architecture.md),
  [domain model](docs/dev/domain-model.md),
  [workflow schema](docs/dev/workflow-schema.md).

## License

See [`LICENSE`](LICENSE).
