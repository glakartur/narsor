# Local tasks in Docker

By default the `local` backend runs a task's runner as a process on your
host: it runs as your user, sees your whole file system and, with
`controller.local.home: host`, uses your own HOME and logins. The **Docker
work mode** runs the working runner of each local task in a container on the
local Docker daemon instead. The container has only the task's storage and a
few read-only files mounted, a private HOME, and the tools of an image that
Narsor builds from your definition.

The Docker work mode is a setting of the local controller. Nothing changes
for workflows, the TUI, the CLI or the web interface.

## Requirements

- Linux with a **rootful** Docker daemon that your user can reach (`docker
  info` works without `sudo`). Rootless Docker and Podman are not supported:
  the controller refuses to start a task on them.
- A trusted local daemon only. Do not point `DOCKER_HOST` at a remote or
  shared daemon: Narsor mounts host paths into the container by their host
  paths.
- The local Narsor daemon as the control plane (trusted mode). A controller
  that serves a control plane beyond this machine refuses the Docker mode.
- An image base with **glibc 2.39 or newer** on the host's architecture
  (Debian 13 "trixie", Ubuntu 24.04 or newer). The runner binary of the host
  is mounted into the container and runs there.

> **Warning — the `docker` group is root on the host.** A user who can talk
> to the Docker daemon can start a privileged container that mounts `/`. Add
> only users to that group whom you would give `sudo`. Narsor's own
> containers are hardened (see [What a container gets](#what-a-container-gets)),
> but that does not limit what the group itself allows.

## Setup

1. Define the task image in the catalogue, `{catalog}/runtime.yaml` (see
   [The task image](#the-task-image)). The example catalogue has one:
   `examples/catalog/runtime.yaml` with scripts in
   `examples/catalog/components/`.
2. Store the tokens the components need (see [Tokens](#tokens)):

   ```console
   $ claude setup-token                     # prints a long-lived token
   $ narsor-cli secret set claude-token     # paste it, then Ctrl-D
   ```

3. Turn the mode on in the configuration file and restart the daemon
   (`narsor-cli daemon stop`; the next client command starts it again):

   ```yaml
   controller:
     local:
       mode: docker
       docker:
         update_interval: 24h   # 0: check at every task start; none: never re-check
   ```

4. Create a task as usual. The first start builds the image in the
   background; the task shows as provisioning until the image is ready.
   Check the settings with `narsor-cli config show`.

In Docker mode the HOME of the tools is always the **private HOME** on task
storage. When `controller.local.home` is not set, Docker mode uses
`private`; an explicit `controller.local.home: host` together with
`mode: docker` is an error, and the daemon does not start.

## The task image

The task image is defined once, in `runtime.yaml` under `task_image`: a base
image and a list of components installed on it, in order.

```yaml
# {catalog}/runtime.yaml
task_image:
  base: debian:trixie-slim
  components:
    - name: base-tools                       # [a-z0-9][a-z0-9-]*, unique
      install: components/base-tools.sh      # a path inside the catalogue
    - name: claude-code
      install: components/claude-code/install.sh
      version: components/claude-code/version.sh
      secrets:
        - { name: claude-token, env: CLAUDE_CODE_OAUTH_TOKEN }
      home:
        - { from: ~/.claude/skills, to: .claude/skills }
    - name: hello
      install: { inline: "echo hello > /usr/local/share/hello" }
```

- **`base`** — any image reference. A tag is pulled again at every version
  check; pin a digest (`debian:trixie-slim@sha256:…`) for repeatable builds.
- **`install`** — a shell script, run with `sh` as root during the build, one
  image layer per component. A bare string is a path relative to the
  catalogue directory (it must stay inside it); `{ inline: "…" }` is the
  script text itself. The scripts are read when a task is created and sealed
  into the task, so a later edit affects only new tasks.
- **`version`** (optional) — a script that prints the version to install. It
  runs **on the host**, not in the image, with only `PATH`, `HOME` and
  `LANG`, for at most 60 s; its output (up to 1 KiB) becomes the build
  argument `NARSOR_<NAME>_VERSION` of the component's layer and the
  `NARSOR_COMPONENT_VERSION` variable of its `install` script. A new version
  rebuilds that layer and the ones after it. A component without a `version`
  script is rebuilt only when the base or an earlier layer changes.
- **`secrets`** (optional) — token store entries the component needs, each
  delivered as an environment variable (`env: NAME`) or as a file under the
  private HOME (`file: relative/path`). Names such as `PATH`, `LD_*`,
  `NARSOR_*` and the harness API key variables are refused.
- **`home`** (optional) — host files or directories (`from`, absolute or
  starting with `~`) copied into the private HOME (`to`, relative) at every
  start of the runner. They are mounted read-only; a target is replaced as a
  whole each time. A source that does not exist on the host refuses the
  task. `/` and the Docker socket are refused as sources.

**The runner's contract.** The runner checks the image before any work, as
on Kubernetes: `/bin/sh`, **`git`** and a **CA bundle** must be present (on
Debian: `apt-get install git ca-certificates`). An image without them fails
the task with a `base_image_contract` problem. The example's `base-tools`
component installs them.

The build adds a passwd and group entry for your uid and gid (named
`narsor`) when the base has none, so tools that look up the user work.

The scripts of the example are a starting point, not a supported
distribution:

- `components/claude-code/` installs Claude Code system-wide in
  `/usr/local/bin` from the official release bucket (checksum verified) and
  turns its self-update off (`DISABLE_AUTOUPDATER` in
  `/etc/claude-code/managed-settings.json`): the image decides the version.
- `components/glab/` installs the GitLab CLI from its release package.

### Updates

The controller keeps one image per definition. At a task start it uses the
cached image while the last version check is younger than
`controller.local.docker.update_interval` (24 h by default; `0` checks at
every start; `none` never checks again after the first build). When the
check is due, a background job pulls the base, runs the `version` scripts and
rebuilds only when something changed. A task waits for that job; a running
task keeps its image.

To check now, without waiting for the interval:

```console
$ narsor-cli image refresh
```

The next task start then checks every definition again.

A **resumed** task runs on the image it last ran on, as long as that image
exists and was built from the same definition, so a resume never changes the
tools under a running piece of work. A new task gets the current image.

A build that fails is reported in the controller log (in the daemon log,
prefix `narsor-controller:`). When the cause can pass on its own (the Docker
daemon or the registry did not answer, a `docker` call timed out), the task
keeps waiting and the build runs again after 60 s. When an install script or
the definition failed, the task's workload ends and the next resume is
refused with the build message until you fix the cause and run
`narsor-cli image refresh`.

Every `docker` call has a deadline: 15 minutes for a pull or a build, 30 s for
the other calls. A call that does not end in time is stopped and counts as a
daemon that does not answer, so the controller tries again later. A daemon
that does not answer when a task starts delays the task; it does not fail it.

## Tokens

The container never sees your host logins. A component gets a token from the
**token store** of the local controller:

```console
$ narsor-cli secret set gitlab-token < token.txt
$ printf '%s' "$TOKEN" | narsor-cli secret set gitlab-token
```

The value comes from standard input, never from the command line. The store
is `<server.state>/local-controller/secrets/` (directory `0700`, files
`0600`). At each start the controller copies the tokens a task needs into the
task's workload directory and mounts them read-only; the runner delivers each
one as the component asks. A task whose token is missing is refused before
anything starts, with the name of the missing entry.

A token delivered as `env` is in the environment of every step of the task,
agent and script steps alike.

### Claude Code

Claude Code in the container logs in with a long-lived OAuth token:

1. On the host, run `claude setup-token` and follow the login.
2. Store the token: `narsor-cli secret set claude-token`.
3. Deliver it in the component: `{ name: claude-token, env:
   CLAUDE_CODE_OAUTH_TOKEN }`.

Limits of such a token: it covers inference only, so **claude.ai connectors**
(the integrations configured in your claude.ai account) are not available in
the container. Add the MCP servers a task needs to the image or to a home
source instead.

> **Licensing.** Using your own subscription with the unmodified Claude Code
> binary is allowed by its terms. Running many unattended tasks in parallel
> for a company, on one person's subscription, may not be: have a licensing
> or legal specialist confirm the use before you rely on it.

## What a container gets

Each working runner is one container, named after its workload and labelled
`io.narsor.managed=true`, `io.narsor.workload=<uid>` and
`io.narsor.task=<task id>`. It is started with:

- `--init`, `--cap-drop ALL`, `--security-opt no-new-privileges`,
  `--pull never`, and your uid and gid (`--user`);
- `--network host` — the runner reaches the local daemon on loopback, and the
  tools have the host's network;
- mounts at the **same paths** as on the host: the task storage
  (read-write); the workload directory, the configuration file, the secrets
  directory, local workspace sources and home sources (read-only, home
  sources under `/run/narsor/home/`); the host's runner binary at
  `/opt/narsor/bin/narsor-runner` (read-only);
- an environment of `TERM`, `LANG`, `LC_*`, `TZ`, the proxy variables and
  the variables the controller itself sets. Nothing else of your environment
  reaches it.

The read-only runner that reads a stopped task (D63) stays a host process in
both modes; it runs no tools.

When a task that last ran in a container starts in `process` mode, the
controller logs a warning: the task then runs as a host process with your
HOME settings, and tools installed only in the image are missing.

> **Warning: switching back to `process` mode runs what the agent left.** A
> task that ran in a container wrote its workspace and its private HOME
> without control. In `process` mode the same files are used by a host
> process with your rights: Git hooks (`.git/hooks/*`), Git settings such as
> `core.fsmonitor` or `core.hooksPath`, and Claude Code hooks in
> `.claude/settings.json` then run on your host. Only the log warning above
> guards this. Before you switch the mode, inspect such tasks or archive
> them.

## Limits

- **No SSH.** There are no SSH keys in the container and `~/.ssh` is not
  mounted. Clone and push over HTTPS with a token and a credential helper,
  for example `glab auth setup-git` or `gh auth setup-git` with the token
  from the store.
- **Host network.** The container shares the host's network: it can reach
  every service the host can, loopback included. It also shares the
  **abstract Unix sockets** of the host (for example an X11 display at
  `@/tmp/.X11-unix/X0` or a D-Bus session bus on an abstract address). The
  container runs with your uid, so a service that accepts connections by
  peer uid accepts the agent too: it can send keystrokes to your X session
  or ask your session bus to start programs on the host. Do not run Docker
  mode on a machine with such a desktop session if that is not acceptable.
- **Proxy variables are visible.** The proxy variables (`http_proxy`,
  `HTTPS_PROXY`, …) are given to `docker run` as arguments, so other users
  of the host can read them in `ps`, and members of the `docker` group in
  `docker inspect`. Use proxy URLs without credentials
  (`http://proxy:3128`, not `http://user:password@proxy:3128`).
- **No per-step images.** A workflow state with its own `image` is refused in
  Docker mode: one task runs in one container.
- **glibc 2.39 or newer** in the image. When the runner cannot start in the
  image (an old glibc, another architecture), the controller log says so;
  the control plane sees only that the workload exited.
- **Supply chain.** The base tag and the install scripts decide what runs
  with your files. A tag can move, and a script that downloads `latest`
  installs whatever is published at build time. Pin digests and versions
  where it matters, and verify checksums as the example scripts do.

## Cleaning up

The controller removes old task images by itself: at its start and after a
new image is first used, it removes the images it built (label
`io.narsor.controller`) that no task uses and that are not the latest image
of a definition in use. A definition is in use while a task runs on, or last
ran on, one of its images, or while it was requested within the update
interval (at least one day; one week with `update_interval: none`). A
definition out of use loses its cached image; the next task that needs it
builds it again. Images of another controller on the same Docker daemon are
never removed.

To remove everything Narsor left (stop the daemon first, or running tasks
lose their containers):

```console
$ docker ps -a --filter label=io.narsor.managed -q | xargs -r docker rm -f
$ docker images --filter label=io.narsor.managed -q | xargs -r docker rmi
```

Docker's **build cache** is not removed: it cannot be filtered by label, and
pruning it all would also remove your own build cache. Remove it yourself
when you need the space:

```console
$ docker builder prune
```
