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
| `local` | `narsor`, `narsor-cli`, `narsor-server`, `narsor-controller`, `narsor-runner` | Run the TUI and CLI with the local server and controller. |
| `frontend` | `narsor`, `narsor-cli` | Connect a TUI/CLI to a separately operated server. |
| `server` | `narsor-server`, `narsor-controller`, `narsor-runner` | Run the network control plane and a separately enrolled workstation backend. Add `--with-frontend` to install the TUI and CLI too. |

The installer script downloads and verifies the archive, copies the binaries,
and, only after consent, adds the binary directory to `~/.bashrc`. It then
runs `narsor-cli init`, which only creates or updates the configuration. The
script asks for the role (arrow keys and Enter) and the binary directory
(default `~/.local/bin`, Tab completes); `narsor-cli init` asks for the Narsor
data directory (default `~/.narsor`). The prompts read from the terminal, so
they also work with `curl ... | bash`.

The data directory is a root: the configuration gets
`catalog_dir: ROOT/catalog`, `state_dir: ROOT/state` and `work_dir: ROOT/work`,
followed by the other settings as commented-out defaults (remove the `#` to
change one). The number of tasks processed in parallel is the global
`max_concurrent_tasks` setting (default 1).

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
non-interactive mode; `--data-dir` defaults to `~/.narsor`. For `server`, also supply:

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
`narsor-runner`) of your user are already running, the installer lists them
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
`narsor-cli purge`, so it needs the binaries to still be present.

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

Server installation configures `server_mode: network`, proxy TLS termination,
the trusted proxy list, and a separate `kind: local`,
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
local token source configured. If no `owner_subject` was configured, claim the
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

Observers poll external sources (Jira, GitHub, GitLab) with a shell script
on a schedule. The control plane keeps the schedule; a separate worker
process, `narsor-cli observer worker`, claims the runs that are due, runs
their scripts and reports the results. Run it on a host that can reach both
the control plane and the sources, under its own system user. How to write
an observer and its script is in [Observers](../user/observers.md).

**Create the worker credential.** The worker does not use the owner's token.
It has a credential of its own, which can only claim the runs of the owner's
observers and report their results. As the owner:

```bash
narsor-cli observer worker-token create --name "$(hostname)" \
  --output /etc/narsor/observer-worker/credential
chown narsor-observer: /etc/narsor/observer-worker/credential
```

The credential is shown or written once; the server keeps only its SHA-256
digest. `--output` creates a new file of mode `0600`. The worker refuses to
start when the file can be read by any other user. `narsor-cli observer
worker-token list` shows the credentials, and `narsor-cli observer
worker-token revoke WORKER_ID` stops one at once: the worker then exits with
an error. A run it claimed and did not report is recorded as lost at the
run's deadline (script limit plus one minute) and does not count as a failed
run of the script.

**Provide the secrets.** An observer names the secrets it needs (a name and
an environment variable); their values never reach the control plane. Put
one file per secret name in the worker's secrets directory, which must be
accessible to the worker's user only (`0700`):

```bash
install -d -m 0700 -o narsor-observer /etc/narsor/observer-worker/secrets
install -m 0600 -o narsor-observer /dev/stdin \
  /etc/narsor/observer-worker/secrets/jira-token <<<"$JIRA_TOKEN"
```

A run whose secret file is missing fails before its script starts. A dry run
of a script version that is not approved gets no secrets unless the owner
asked for them (`--with-secrets`).

**Start the worker.** The unit
[`deploy/observer-worker/narsor-observer-worker.service`](../../deploy/observer-worker/narsor-observer-worker.service)
runs the worker as `narsor-observer` and every script as a second system
user, `narsor-observer-script`, which cannot read the credential or the
secrets directory. It grants the worker only the capabilities needed to
switch to that user and end its processes (`CAP_SETUID`, `CAP_SETGID`,
`CAP_CHOWN`, `CAP_KILL`), and bounds the worker and all its scripts together
with `MemoryMax` and `TasksMax`. Create both users, set `NARSOR_SERVER` in
`/etc/narsor/observer-worker.env`, then enable the unit as its header
describes. Without systemd, start the worker the same way:

```bash
NARSOR_SERVER=https://narsor.example.com narsor-cli observer worker \
  --credential-file /etc/narsor/observer-worker/credential \
  --secrets-dir /etc/narsor/observer-worker/secrets \
  --script-user narsor-observer-script
```

The worker refuses to start without `--script-user`. For a trial on a
developer machine only, `--insecure-same-user` runs scripts as the worker's
own user; a script can then read the worker credential and every observer's
secrets, and the worker warns about it at start.

`--once` claims once and exits, for a cron job. `--work-dir` moves the run
directories out of the system's temporary directory; `--script-path` sets
the scripts' `PATH`.

**What a script gets.** The worker asks for a due run every 10 seconds. For
each run it makes a temporary directory (mode `0700`, owned by the script
user, removed afterwards) and runs `sh` there with an empty environment plus
exactly these variables:

| variable | value |
| --- | --- |
| `PATH` | the directory of `narsor-cli`, then `/usr/local/bin:/usr/bin:/bin` |
| `HOME` | the run directory |
| `NARSOR_SERVER` | the control plane's URL |
| `NARSOR_TOKEN` | the run's token: valid on this run's effect routes only, until its result or deadline |
| `NARSOR_OBSERVER_RUN` | the run's id |
| one per secret | the value of the observer's secret |

Nothing of the worker's own environment is passed on. The script reads the
run's input as JSON on stdin and prints its result on stdout. It runs in a
process group of its own: at its time limit (5 minutes) the group gets
`SIGTERM`, then `SIGKILL` 5 seconds later; when the script exits, anything it
left running is killed. A process can leave the group (`setsid`), so after
each run the worker also kills every process of the script user, and it
claims no run while a process of that user still runs. Thus a process left
behind cannot read the secrets and the token of the next run. Use a
dedicated script user that runs nothing else. stdout may hold at most 1 MiB (more is a format
error that leaves the observer's state unchanged); the first 64 KiB of stderr
are kept. Before the result leaves the worker, control characters are
removed and every occurrence of a secret value or of the run's token is
replaced by `[masked]`, error messages included. Masking is a second line of
defence: scripts should not print secrets in the first place.
