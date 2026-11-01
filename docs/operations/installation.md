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
curl -fsSL https://github.com/glakartur/narsor/releases/download/v0.4.0/install.sh | bash
```

The URL pins a release tag because every release is currently published as a
pre-release, and GitHub's `/releases/latest/download/` URL ignores
pre-releases (it returns 404). The tag is bumped with each release (see
`CONTRIBUTING.md`). The script itself still defaults to `latest` when run
without a version.

To select a different version, pass its tag to the script:

```bash
curl -fsSL https://github.com/glakartur/narsor/releases/download/v0.4.0/install.sh |
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
curl -fsSL https://github.com/glakartur/narsor/releases/download/v0.4.0/install.sh |
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
curl -fsSL https://github.com/glakartur/narsor/releases/download/v0.4.0/install.sh |
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

`--update-bashrc` explicitly opts into adding the binary directory to
`~/.bashrc`. Without it, non-interactive installation never changes a shell
startup file. The line carries a `# narsor-installer` marker so uninstall can
find it. To build from source, see the README.

## Uninstall

The same script removes what it installed:

```bash
curl -fsSL https://github.com/glakartur/narsor/releases/download/v0.4.0/install.sh |
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
