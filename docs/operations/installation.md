# Linux installation and server operation

The GitHub Release installer installs prebuilt Narsor binaries and configures
them for local, frontend, or server use. It requires `curl` or `wget`,
`sha256sum`, and a Linux x86_64 host; it does not require Rust or GitHub CLI.

The release target is `x86_64-unknown-linux-gnu`, built on Ubuntu 22.04. It
requires a compatible glibc. A musl-only system such as Alpine is not supported
by this artifact.

## Install a role

Run the installer of the newest release and follow its prompts:

```bash
curl -fsSL https://github.com/glakartur/narsor/releases/download/v0.14.3/install.sh | bash
```

The URL pins a release tag because every release is currently published as a
pre-release, and GitHub's `/releases/latest/download/` URL ignores
pre-releases (it returns 404). The tag is bumped with each release (see
`CONTRIBUTING.md`). The script itself still defaults to `latest` when run
without a version.

To select a different version, pass its tag to the script:

```bash
curl -fsSL https://github.com/glakartur/narsor/releases/download/v0.14.3/install.sh |
  bash -s -- v1.2.3
```

The installer verifies the archive checksum before extracting it. The checksum
is published alongside the archive, so this detects corruption but is not
independent publisher authentication. To verify the GitHub build provenance
yourself, download the archive and `provenance.jsonl` release asset, then use a
recent GitHub CLI:

```bash
gh attestation verify narsor-linux-x86_64-gnu.tar.gz \
  --bundle provenance.jsonl \
  --repo glakartur/narsor \
  --signer-workflow glakartur/narsor/.github/workflows/release.yml
```

Choose one role:

| Role | Installed programs | Intended use |
| --- | --- | --- |
| `local` | `narsor`, `narsor-cli`, `narsor-web`, `narsor-server`, `narsor-controller`, `narsor-runner`, `narsor-observer-worker` | Run the TUI, CLI and browser interface with the local server and controller. |
| `frontend` | `narsor`, `narsor-cli`, `narsor-web` | Connect a TUI/CLI or the browser interface ([`narsor-web`](../user/web.md)) to a separately operated server. |
| `server` | `narsor-server`, `narsor-controller`, `narsor-runner`, `narsor-observer-worker` | Run the network control plane and a separately enrolled workstation backend. Add `--with-frontend` to install the TUI, CLI and `narsor-web` too. |

The installer script downloads and verifies the archive, copies the binaries,
and, only after consent, adds the binary directory to `~/.bashrc`. It then
runs `narsor-cli init`, which only creates or updates the configuration. The
script asks for the role (arrow keys and Enter) and the binary directory
(default `~/.local/bin`, Tab completes); `narsor-cli init` asks for the Narsor
data directory (default `~/.narsor`). The prompts read from the terminal, so
they also work with `curl ... | bash`. Each step (download, verification,
role, binary directory, configuration, installation, `PATH`) starts with its
own `==>` heading, and the last line names the installed version.

Every program reports the version of its release: `narsor-cli --version`
prints `narsor-cli 1.2.3` for the release tagged `v1.2.3`. The archive's
`manifest.json` carries the same version, and the installer refuses an
archive whose version does not match the tag it asked for.

The data directory is the root: the configuration gets `global.root: ROOT`
and the `server` section with the relative directories `catalog`, `state`
and `work` (they resolve to `ROOT/catalog`, `ROOT/state`, `ROOT/work`),
followed by the other settings as commented-out defaults inside
their sections (remove the `#` to change one; each comment names the flat key
the setting replaces). The number of tasks processed in parallel is the
`server.max_concurrent_tasks` setting (default 1). The file is a tree of
sections, one per program; see [Configuration](../user/configuration.md#the-tree-of-sections).
`ROOT/state` and `ROOT/work` are roots: each program keeps its files in a
directory of its own below them (`state/narsor-server/`,
`state/narsor-controller-local/`, `work/narsor-controller-local/`, …), and
the server and its clients meet in `state/run/`; see
[The on-disk layout](../user/configuration.md#the-on-disk-layout). An
installation of an older version is moved to this layout once, at the first
start of the new server; see
[Updates](updates.md#update-to-a-directory-per-program-state-ownership).

For the `local` role, `narsor-cli init` also asks for the **work mode for
local tasks**: `process` (the default: tasks run on the host as your user) or
`docker` (tasks run in a hardened container; needs rootful Docker). Pass
`--local-mode process|docker` to answer without the question; in
non-interactive mode without the option the mode stays `process` and nothing
is written. When you choose `docker`, `init` checks the Docker daemon, writes
`controller.local.mode: docker` and, when the catalogue has no
`runtime.yaml` yet, creates one with the example task image (Debian with
`base-tools` and `claude-code`) and its component scripts. An existing
`runtime.yaml` is never changed. If Docker is not ready, the interactive
question asks whether to write the docker mode anyway (default: no, use
`process`); an explicit `--local-mode docker` is written with a warning. The
`server` role refuses `--local-mode docker`: its workstation controller serves
the network server, and the Docker mode needs the local daemon as the control
plane. See [Local tasks in Docker](../user/local-docker.md) for the next
steps (the Claude Code token and the tools of the image).

If a configuration file already exists, it is left untouched. The installer
writes `config.yaml.new` next to it with the default values and does not ask
for the data directory (use `--data-dir` to choose another root); compare the
two files and copy over what you need.

For container builds, provide the role and directories so the installer never
prompts. The configuration path is optional; when omitted, the effective
configuration file is used, or the user's default config path if none exists.
Use `--config-file` to choose another file explicitly:

```bash
curl -fsSL https://github.com/glakartur/narsor/releases/download/v0.14.3/install.sh |
  bash -s -- --role frontend --non-interactive \
    --bin-dir /usr/local/bin \
    --data-dir /var/lib/narsor \
    --config-file /etc/narsor/config.yaml
```

For `local` and `frontend`, the installer needs `--role` and `--bin-dir` in
non-interactive mode; `--data-dir` defaults to `~/.narsor`. For `local`, add
`--local-mode docker` to run local tasks in containers. For `server`, also supply:

* `--server-bind` with an address reachable by the corporate proxy, not a
  loopback address;
* `--oidc-issuer` using HTTPS and `--oidc-audience`;
* one or more `--trusted-proxy` IP addresses or CIDR ranges;
* a unique `--backend-id` other than `local`.

For example, on a dedicated VM behind an existing corporate proxy:

```bash
curl -fsSL https://github.com/glakartur/narsor/releases/download/v0.14.3/install.sh |
  sudo bash -s -- --role server --non-interactive \
    --bin-dir /usr/local/bin \
    --data-dir /var/lib/narsor \
    --config-file /etc/narsor/config.yaml \
    --server-bind 10.20.0.5:8080 \
    --oidc-issuer https://login.example.com/realms/narsor \
    --oidc-audience narsor-api \
    --trusted-proxy 10.20.0.12/32 \
    --backend-id workstation
```

Replace the example addresses and identity settings with the values supplied
by the network and identity operators. The proxy must terminate HTTPS, forward
the original authorization header, set `X-Forwarded-Proto: https`, and be the
only network path to the API. Network policy that restricts the server to the
corporate network is external to Narsor.

The installer respects effective `NARSOR_*` settings. An environment value
overrides the same file setting; explicit installer options that disagree
with the effective value are rejected. If an effective system-wide config is
not writable, the interactive installer asks for another target. A
non-interactive invocation must use a writable `--config-file`.

If Narsor processes (`narsor`, `narsor-server`, `narsor-controller`,
`narsor-runner`, `narsor-observer-worker`) of your user are already running, the installer lists them
before it replaces the binaries and asks whether to stop them. Stopping them
interrupts running tasks. A stopped process gets `SIGTERM` and 10 seconds to
exit, then `SIGKILL`. If you answer no, they keep running the previous version
until you restart them. Non-interactive installation never stops anything
unless you pass `--stop-running`; without it the installer only prints the
list. `--stop-running` also skips the question in an interactive run.

`--progress` shows a progress bar while the release archive downloads
(`curl --progress-bar`, or `wget --show-progress`); without it the download is
silent.

`--update-bashrc` explicitly opts into adding the binary directory to
`~/.bashrc`. Without it, non-interactive installation never changes a shell
startup file. The line carries a `# narsor-installer` marker so uninstall can
find it. To build from source, see the README.

The installer does not install agent harness CLIs. Install the ones your
workflows use (Claude Code `claude`, `codex`, `opencode`, the GitHub Copilot
CLI `copilot`) on the host or in the task image that runs `narsor-runner`,
and put them on `PATH`. For Copilot, `COPILOT_CLI_PATH` can name the CLI
instead. The GitHub Copilot SDK in `narsor-runner` targets Copilot CLI
1.0.104; see [Agents](../user/agents.md).

## Uninstall

The same script removes what it installed:

```bash
curl -fsSL https://github.com/glakartur/narsor/releases/download/v0.14.3/install.sh |
  bash -s -- --uninstall
```

It deletes only the five Narsor binaries from the binary directory
(`~/.local/bin` unless `--bin-dir` is given) and the marked `~/.bashrc` line.
Configuration and data stay in place. Pass the same `--bin-dir`,
`--data-dir`, and `--config-file` values you used to install.

`--purge` also removes the configuration file, its `.bak.*` backups and generated `.new` file, and the
data directory (`--data-dir`, default `~/.narsor`). It first lists
what it will delete and asks for confirmation; with `--non-interactive`, the
`--purge` flag is the confirmation. Directories that the configuration names
outside the data directory are listed and kept, and a data directory that
contains your home directory is refused. `--purge` runs the installed
`narsor-cli purge`, so it needs the binaries to still be present. A
controller started by hand keeps its state below its own `--state-root`
(by default `$XDG_STATE_HOME/narsor/narsor-controller-local/`) and its
task storages below `--work-root`; `--purge` does not remove them.

## Frontend connection profile

The server operator can export a profile containing only its HTTPS URL:

```bash
narsor-cli connection-profile export \
  --server https://narsor.example.com \
  --output narsor-connection.yaml
```

Transfer that file to the frontend host and import it. Interactive import
prints the normalized endpoint and asks whether to trust its HTTPS origin:

```bash
narsor-cli connection-profile import narsor-connection.yaml
```

For a non-interactive import, explicitly accept the endpoint:

```bash
narsor-cli connection-profile import narsor-connection.yaml --trust-server
```

The imported endpoint is saved to
`$XDG_CONFIG_HOME/narsor/connection.yaml` (default
`~/.config/narsor/connection.yaml`). The file contains no token or credential.
The TUI and CLI continue to get OIDC tokens from the frontend user's own
`NARSOR_TOKEN_FILE`, `NARSOR_TOKEN_COMMAND`, or `NARSOR_TOKEN` source.
`--server` takes precedence over `NARSOR_SERVER`; either takes precedence over
the imported profile. If none is set, local-daemon discovery remains in use.

## Start the server and workstation controller

Server installation configures `server.mode: network`, the `server.oidc`
group, proxy TLS termination and the trusted proxy list (`server.tls`), and a separate `kind: local`,
`trust_scope: workstation` backend in the catalogue. It prints commands to
start the server with the selected config file and the controller separately.
No systemd unit or Kubernetes resources are installed; the VM's existing
runtime is responsible for starting and supervising both processes.

**Workstation tasks run as the VM account without a container.** The server
owner can therefore ask the controller to run programs and access every file
that account can read. Keep this on a dedicated VM with a minimally privileged
account and no unrelated credentials.

Run the owner commands from an installed CLI (`--with-frontend` adds it to the
server host) with `NARSOR_SERVER` set to the corporate HTTPS URL and the owner's
local token source configured. If no `server.owner_subject` was configured, claim the
server's one-time owner code before creating the workstation enrollment code:

```bash
export NARSOR_SERVER=https://narsor.example.com
export NARSOR_TOKEN_FILE="$HOME/.cache/narsor/token"
narsor-cli auth claim '<claim-code>'
```

The owner enrollment command printed by the installer retains the explicit
risk acknowledgement and names the remote server endpoint. Replace its
`<corporate-server>` placeholder; keep the owner's `NARSOR_TOKEN_FILE` or
`NARSOR_TOKEN_COMMAND` configured:

```bash
NARSOR_SERVER='https://<corporate-server>' \
  narsor-cli controller enroll-code workstation --accept-workstation-risk
```

The controller connects outbound to the corporate HTTPS endpoint. Supply its
enrollment code through `NARSOR_ENROLLMENT_CODE` or a protected file, never as
a command-line argument. See [Remote execution backends](remote-backends.md)
for proxy requirements, token handling, controller credentials, and the full
workstation security boundary.

## Run the observer worker

Automations of the catalogue read external sources (Jira, GitHub, GitLab)
with shell scripts, on the schedule of their queue or when a gate enters or
leaves the queue. The control plane decides when a script runs; a worker
process, `narsor-observer-worker`, claims the runs that are due, runs their
scripts and reports the results. The server starts the worker and
supervises it. How to write an automation and a queue is in
[Automations and queues](../user/observers.md). Before an update from
observers stored in the state to catalogue automations, read
[Updates](updates.md#update-to-catalogue-automations-d78).

**Enable the worker.** The worker is off by default. Set the
`server.observer` group in the server's configuration file (or the
`NARSOR_SERVER__OBSERVER__<KEY>` environment variables, for example
`NARSOR_SERVER__OBSERVER__ENABLED=true`):

| setting | value |
| --- | --- |
| `server.observer.enabled` | `true` starts the worker; `false` (default) does not |
| `server.observer.script_user` | the system user that runs the scripts; not the server's user, not `root` |
| `server.observer.secrets_dir` | the absolute path of the secrets directory; empty: no secrets |
| `server.observer.server_url` | the URL the worker and its scripts use for the API; empty: the server's own address (trusted mode only) |
| `server.observer.insecure_same_user` | `true` runs the scripts as the server's own user, with the server's environment (as a runner runs its agent); for a trial or a single-user machine only |

```yaml
server:
  observer:
    enabled: true
    script_user: narsor-observer-script
    secrets_dir: /etc/narsor/observer-secrets
    # Network mode only: the URL of the TLS proxy.
    server_url: https://narsor.example.com
```

The server refuses to start when `server.observer.enabled` is `true` and
neither `server.observer.script_user` nor
`server.observer.insecure_same_user: true` is set, or both are set. In
network mode the server refuses to start without
`server.observer.server_url`: every request must come through the TLS proxy,
the worker's requests too. These checks run before the server starts
anything, and `narsor-cli config show` reports them too.

The server finds `narsor-observer-worker` in its own directory, as it finds
`narsor-controller`. Scripts call `narsor-cli` for their effects (task
create, `task decide`), from the same directory: on a `server` install add
`--with-frontend`, or copy `narsor-cli` next to `narsor-server`. The worker runs as the server's user. When it stops,
the server starts it again after a pause that doubles up to 30 seconds. When
the server stops, it stops the worker first. A run that the worker did not
report is recorded as lost at the run's deadline (script limit plus one
minute) and does not count as a failed run of the script.

**The worker key.** The worker does not use the owner's token. At every
start of the worker, the server makes a new random key, keeps only its
SHA-256 digest in memory, and gives the key to the worker in the
`NARSOR_OBSERVER_WORKER_KEY` environment variable (not as an argument, so
`ps` does not show it). The key can only claim due runs and report their
results. No file holds the key. A restart of the worker or of
the server makes the old key invalid. There is no command to make or revoke
a worker key.

**Provide the secrets.** An automation names the secrets it needs in its
`automation.yaml` (a name and an environment variable); their values never
reach the control plane or the catalogue. The installation has one owner,
so the secrets directory is flat: one file for each secret name. A script
gets only the secrets its automation declares. The directory must be
accessible to the server's user only (`0700`); the worker refuses to start
otherwise. In this example the server runs as the user `narsor`:

```bash
install -d -m 0700 -o narsor /etc/narsor/observer-secrets
install -m 0600 -o narsor /dev/stdin \
  /etc/narsor/observer-secrets/jira-token <<<"$JIRA_TOKEN"
```

When a declared secret file is missing, the run fails before its script
starts, and its error names the secret. Earlier versions had one
subdirectory per owner (`{secrets dir}/{owner}/{name}`): move the files up
into the directory itself.

**Isolate the scripts.** Create a second system user for the scripts, which
runs nothing else:

```bash
useradd --system --no-create-home --shell /usr/sbin/nologin narsor-observer-script
```

The script user cannot read the secrets directory or the worker's
environment. To switch to the script user, give it its run directory and
stop its processes, the worker needs the capabilities `CAP_SETUID`,
`CAP_SETGID`, `CAP_CHOWN` and `CAP_KILL`. Because the server starts the
worker, give these capabilities to the server's service (for systemd:
`AmbientCapabilities=` and `CapabilityBoundingSet=`). At start the server
removes them from its own ambient and effective sets and keeps them only in
its permitted and inheritable sets. Only the worker gets them again, as
ambient capabilities, and only with a script user. Other processes that
the server starts (the local controller, its runners and the agents) get
no capability. The worker clears them and sets `no_new_privs` in each
script process before the script starts, so a script has none of them.
Limits of the server's service (`MemoryMax`, `TasksMax`) apply to the
server, the worker and all scripts together.

With `server.observer.insecure_same_user: true` scripts run as the server's own
user. A script can then read the worker key, every secret and the server's
state. The worker writes a warning at start. Do not use it in
production.

In this mode isolation gains nothing, so the script also gets the
environment of the server, as an agent of a runner does: the user's
`HOME`, and with it the logins of `glab`, `gh` and other tools, and the
user's `PATH` after the worker's own. A script can then use `glab api`
without a secret. The run's variables in the table below and the declared
secrets take precedence. Variables named `NARSOR_*` (the worker key among
them) are never passed on.

**What a script gets.** The worker asks for a due run; the server holds the
request up to 25 seconds until a run is due, so a script starts soon after
it is due. After an empty answer that came at once, the worker waits
2 seconds before it asks again. For each run it makes a temporary directory (mode `0700`, owned by the script
user, removed afterwards) and runs `sh` there with an empty environment plus
exactly these variables:

| variable | value |
| --- | --- |
| `PATH` | the directory of `narsor-observer-worker` (and of `narsor-cli`), then `/usr/local/bin:/usr/bin:/bin` |
| `HOME` | the run directory |
| `NARSOR_SERVER` | the control plane's URL |
| `NARSOR_TOKEN` | the run's token: valid on this run's effect routes only, until its result or deadline |
| `NARSOR_OBSERVER_RUN` | the run's id |
| one per secret | the value of a secret that the automation declares |

With a script user, nothing of the worker's own environment is passed on,
the worker key included (for `server.observer.insecure_same_user: true`, see
above). The script reads the
run's input as JSON on stdin and prints its result on stdout. It runs in a
process group of its own: at its time limit (5 minutes for `check`,
120 seconds for `in` and `out`) the group gets
`SIGTERM`, then `SIGKILL` 5 seconds later; when the script exits, anything it
left running is killed. A process can leave the group (`setsid`), so after
each run the worker also kills every process of the script user, and it
claims no run while a process of that user still runs. Thus a process left
behind cannot read the secrets and the token of the next run. Use a
dedicated script user that runs nothing else. stdout may hold at most 1 MiB (more is a format
error that leaves the state of the pair unchanged); the first 64 KiB of stderr
are kept. Before the result leaves the worker, control characters are
removed and every occurrence of a secret value or of the run's token is
replaced by `[masked]`, error messages included. Masking is a second line of
defence: scripts should not print secrets in the first place.
