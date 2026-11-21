---
date: 2026-11-21
topic: local-docker-runner
---

# Local work mode: the runner in a Docker container

## Summary

`narsor-controller local` gets a second work mode, chosen in the Narsor
configuration file: it starts the runner and the harness in a Docker
container instead of in the session of the user who runs `narsor-server`.
The task image is defined once in the configuration: a base image plus
optional components, each with an `install` script and an optional `version`
script. Narsor builds the image locally and keeps it up to date. Secrets are
never put into the image: the container gets tokens as environment
variables, and configuration files without secrets are copied from the host
read-only.

---

## Problem Frame

A local task today runs as a process of the user who runs the daemon
(D57: `controller.local.home: host`). The agent usually runs with
`--dangerously-skip-permissions`, so it can change or delete any file of that
user and read every secret in the user's home: `~/.ssh`, cloud credentials,
tokens of other tools, the history and sessions of every other Claude Code
project. Safe execution of the agent is the reason for this work.

The Kubernetes backend already isolates a task, but it takes the base image
from the repository definition (`base_image`, overridden by the workflow
`globals.image` and a step `image`). That does not work for a workflow with
several repositories: no single repository owns the image the task runs in.

Getting a working environment into a container today is manual. The `personal`
stage of `deploy/kind/images.Dockerfile` copies a filtered `~/.claude` into an
image, so the image carries a login and must stay in the local registry only.

---

## Actors

- A1. Operator: the user who runs `narsor-server` on their workstation; writes
  the configuration, the image definition and gives Narsor the tokens.
- A2. Agent: the harness (Claude Code, codex, …) and the tools it calls inside
  the task container; not trusted with the host.

---

## Key Flows

- F1. First setup of Docker mode
  - **Trigger:** A1 wants local tasks to run isolated.
  - **Actors:** A1
  - **Steps:** A1 writes the image definition (base image, components such
    as Claude Code and glab). A1 runs `claude setup-token` and creates a
    GitLab PAT, then gives both tokens to Narsor once. A1 lists the
    configuration files without secrets to copy from the host (Claude
    settings and skills, `.gitconfig`). A1 sets the local work mode to Docker.
  - **Outcome:** The next task starts in a container on the built image, with
    the harness logged in on A1's subscription and glab authenticated.
  - **Covered by:** R1, R5, R6, R12, R13, R14

- F2. Automatic update of a component
  - **Trigger:** A new release of a component (for example Claude Code)
    appears upstream.
  - **Actors:** none (Narsor)
  - **Steps:** Narsor runs the component's `version` script on its periodic
    check. The result differs from the last build. Narsor rebuilds that
    component's layer and every layer after it. New tasks get the new image.
  - **Outcome:** Running tasks stay on their image; new tasks use the new
    release with no action from A1. A failed build leaves the previous image
    in use and is reported.
  - **Covered by:** R8, R9, R10, R11

---

## Requirements

**Work mode selection**
- R1. The local work mode (`process`: the current behavior, or `docker`) is a
  setting in the Narsor configuration file, in the section read by
  `narsor-controller local`. The built-in value stays `process`.
- R2. The mode is read at every start of a runner, the same way as the HOME
  mode in D57. It is not pinned to the task.
- R3. At start, the daemon writes a warning when tasks that last ran in
  Docker mode will resume in `process` mode (they lose isolation and their
  harness sessions).
- R4. When Docker mode is selected and Docker is not available, or the image
  cannot be built, a task does not start on the host as a fallback. It
  reports a readable problem.

**Isolation**
- R5. In Docker mode, the agent can write only to the task's own storage
  (the working tree and the task's private HOME). No other host path is
  writable from the container.
- R6. In Docker mode, the container sees no host secrets except the ones the
  configuration names explicitly: no `~/.ssh`, no host `~/.claude`, no
  inherited API keys. Network and host-process isolation are the Docker
  defaults; nothing more is required.

**Task image**
- R7. The task image is defined once, globally, in the Narsor configuration:
  a base image reference plus an optional ordered list of components. With
  no components the base image is used as it is.
- R8. A component has a name, an `install` script (inline text or a path to a
  script file) and an optional `version` script that prints the latest
  available version. Narsor builds one layer per component on top of the
  base image.
- R9. Narsor rebuilds a component's layer, and every layer after it, when the
  `install` script changes or the `version` output changes. It rebuilds the
  whole image when the base image tag points to a new digest. A component
  without `version` is rebuilt only when its script changes.
- R10. Narsor checks for updates periodically without operator action, and
  the operator can force a rebuild.
- R11. A task records the digest of the image it starts on (as
  `ResolvedEnvironment` does today); an update affects only tasks created
  afterwards. Narsor removes built images no task uses any more.
- R12. The runner is not part of the image: Narsor mounts the host's
  `narsor-runner` into the container read-only, so its version always matches
  the daemon.
- R13. The global image is the default task image for every backend,
  Kubernetes included. The workflow (`globals.image`) and step `image`
  overrides keep working. The repository-level `base_image` is marked
  deprecated, not removed.

**Secrets and configuration**
- R14. Secrets never go into an image. A token (for example the result of
  `claude setup-token`, a GitLab or GitHub PAT) is given to Narsor once and
  stored in a Narsor-managed location readable only by the operator. The
  container receives it as an environment variable, or as a file when a tool
  requires one.
- R15. Configuration files without secrets (harness settings, skills,
  plugins, `.gitconfig`, tool settings) are named per component and copied
  from the host into the task's private HOME at every container start. The
  source is mounted read-only; changes the agent makes never reach the host.
- R16. For Claude Code, the long-lived subscription token from
  `claude setup-token` is the documented way to log in. The documentation
  states that this token does not give the claude.ai connectors; an MCP
  server a task needs is configured in the copied harness configuration with
  its own token.

---

## Acceptance Examples

- AE1. **Covers R2, R3.** Given a task created in Docker mode and then paused,
  when the operator switches the mode to `process` and restarts the daemon,
  the daemon writes a warning that names the task, and the task resumes on
  the host without its container sessions.
- AE2. **Covers R4.** Given Docker mode and a stopped Docker daemon, when a
  task is started, the task does not run on the host; it shows a problem
  that says Docker is not available.
- AE3. **Covers R9, R11.** Given a running task on image digest D1, when the
  `version` script of Claude Code prints a new version, Narsor builds digest
  D2; the running task stays on D1 and the next new task starts on D2.
- AE4. **Covers R7, R13.** Given an image definition with no components and a
  workflow with two repositories, when the task goes to the Kubernetes
  backend, it runs on the global base image with no repository-level image.
- AE5. **Covers R5, R6.** Given a task in Docker mode, when the agent tries to
  read `~/.ssh/id_ed25519` or write to a host path outside the task storage,
  the path does not exist in the container.

---

## Success Criteria

- The operator runs local tasks with `--dangerously-skip-permissions` and the
  agent cannot read host secrets or change host files outside the task.
- Adding a tool to the task environment means adding one component with an
  install script; the tool then stays current without manual rebuilds.
- Switching between `process` and `docker` is one configuration change.
- Planning can design the image build, the token store and the container
  start without inventing behavior: every open point is listed below.

---

## Scope Boundaries

- Building and pushing an image with components to a registry for the
  Kubernetes backend (Kubernetes uses the global image only when it has no
  components, or a pushed image the operator built).
- Write-back of a refreshed login to the token store, and an interactive login
  session inside a container. Needed later only for logins that rotate their
  refresh token (for example codex on a subscription, or full Claude OAuth
  with claude.ai connectors).
- Network egress restriction and isolation from host services beyond Docker
  defaults.
- Per-user credentials on a shared server: the token store belongs to the one
  operator of a local installation.
- Dev Container Features as the component format (possible later as an
  import).
- Per-step images in Docker mode beyond what the Kubernetes backend supports.

---

## Key Decisions

- Docker is a work mode of the local backend, chosen in the configuration
  file, not a new catalogue backend: this is the operator's explicit choice.
- Mode read at every runner start (like D57), not pinned per task: the
  operator accepts the loss of isolation and sessions on a switch; R3 makes
  it visible.
- One image form (base + optional components) instead of "ready image" vs
  "built image": with no components it is the same artifact Kubernetes uses.
- Own simple component format (`install` + `version`) instead of Dev
  Container Features: no node dependency, and the `version` check gives
  automatic updates, which Features do not have.
- Runner mounted from the host, not built into the image: its version must
  match the daemon, not the image.
- Static tokens instead of copied OAuth logins: a copy of a rotating login
  invalidates the original (D57), so the host console or later tasks would
  lose their login. `claude setup-token` uses the subscription, is valid for
  one year and does not rotate.
- No write-back and no login session in v1: with static tokens neither is
  needed, and the component format leaves room to add write-back later.

---

## Dependencies / Assumptions

- The Anthropic documentation (code.claude.com, legal and compliance) allows
  an end user to sign in to the unmodified Claude Code binary with their own
  subscription, and forbids routing subscription credentials on behalf of
  other users. It does not define whether unattended, orchestrated or
  concurrent sessions are "ordinary use". **Before company use of
  subscription tokens for parallel unattended tasks, a licensing or legal
  specialist (or Anthropic sales) must confirm the interpretation.**
- Docker Engine is installed and the operator can run containers.
- Unverified: whether the host `narsor-runner` binary runs in every base
  image. Today it is linked against glibc 2.39 or newer
  (`deploy/kind/images.Dockerfile`), so a base image with an older or no
  glibc fails unless the runner is built statically.

---

## Outstanding Questions

### Deferred to Planning

- [Affects R1, R7][Technical] Where the image definition lives in the
  configuration tree, given that the Kubernetes backend reads images from the
  catalogue today and section ownership rules (one program per section).
- [Affects R12][Technical] Static (musl) runner build vs a glibc contract on
  the base image; how the runner contract check reports an incompatible image.
- [Affects R5][Technical] How the containerized runner reaches the daemon
  (trusted mode listens on loopback with a local token).
- [Affects R5][Technical] How a local workspace from `local_workspace_roots`
  and the read-only runner for a stopped task (D63) work in Docker mode.
- [Affects R10][Technical] Update check interval and where `version` scripts
  run (host or a throwaway container).
- [Affects R14, R15][Technical] Token store location and format; how the
  operator gives a token (paste, or read from a host file or variable).
- [Affects R2][Technical] Container lifecycle and how D58 orphan cleanup
  identifies a container instead of a process group.
- [Affects R13][Technical] Precedence of the global image vs the repository
  `base_image` while it is deprecated.
