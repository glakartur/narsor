# Configuration

The configuration file, how settings are layered, and where Narsor keeps its data on disk. Architectural background is in [the architecture decisions](../dev/architecture.md).

## Configuration

Every setting has a value built into the binary, so narsor runs with no
configuration at all. What a setting is worth is decided **per setting**, and
the first of these that provides one wins:

| # | Layer | Source shown as |
|---|-------|-----------------|
| 1 | a command-line flag | `flag (--root)` |
| 2 | an environment variable | `env (NARSOR_SERVER__MODE)` |
| 3 | the configuration file | `file (/etc/narsor/config.yaml)` |
| 4 | the value built into the binary | `built-in` |

Per setting, not per file: a file that sets one key does not displace a flag
that sets another. The only setting with a flag is `global.root` (`--root`).

### Where the file is looked for

One file, found by cascade, first hit wins. The files are **not merged** — a
file that loses is shadowed whole:

| # | Where | Source shown as |
|---|-------|-----------------|
| 1 | the path given with `--config` | `flag` |
| 2 | `$XDG_CONFIG_HOME/narsor/config.yaml` (then `.yml`), else `$HOME/.config/narsor/…` | `user` |
| 3 | `/etc/narsor/config.yaml` (then `.yml`) | `system` |

`/etc/narsor/` is the intended mount point of a Kubernetes ConfigMap. The data
directory is deliberately **not** a step of the cascade: the file cannot live
in the directory it configures.

No file in any of those places is a normal, silent state. An **empty** file is
also valid, and means every value is the built-in one — `config show` then
reports that file as the one in force while every source reads `built-in`,
which looks contradictory and is correct.

A candidate that exists but is not a readable file — a directory mounted over
the path, a symbolic link with no target, a mode that denies the read — stops
the start with a message naming the path and the reason. These are the ways a
ConfigMap arrives broken, and running silently on built-in values while a file
sits right there is the failure this layer exists to prevent.

### The tree of sections

The file is a tree. Each top-level key (a **root**) is a section, and each
section belongs to one program:

| Section | Read by |
|---|---|
| `global` | every program |
| `server` | `narsor-server` (the daemon); `server.state` also by `narsor-cli`, `narsor` and `narsor-web` |
| `controller` | `narsor-controller local` |
| `runner` | `narsor-runner` |
| `tui` | `narsor` and `narsor-cli` (which hosts the `tui` command) |
| `web` | `narsor-web` |
| `cli` | `narsor-cli`; it has no settings yet |

**Section ownership.** A program reads `global` and its own sections, and
nothing else, with two named exceptions: the clients (`narsor-cli`, `narsor`,
`narsor-web`) read `server.state` to find the local daemon, and `narsor-cli`
reads the whole `tui` section. A program checks only what it reads, so an
error in the `web` section does not stop the server or a runner, and an error
in `global` stops every program. Each setting has exactly one place in the
tree: there is no inheritance between sections. A group of settings that
belongs to one feature (`server.oidc`, `server.tls`, `server.observer`) has
its own level; `server.observer` has an explicit switch, `enabled`.

```yaml
# ~/.config/narsor/config.yaml — every key is optional
global:
  root: /srv/narsor            # the base directory; --root sets it
  log_level: info              # off, error, warn, info, debug, trace; RUST_LOG wins
server:
  catalog: catalog             # relative to global.root: /srv/narsor/catalog
  state: state                 # executions, discovery file, the registry
  work: /var/tmp/narsor        # absolute: used as is
  agent_harness: claude-code   # for an agent action that names none (unless `narsor-cli harness default set` overrides it)
  max_concurrent_tasks: 10     # tasks processed at once across all backends; `none` removes the limit; `L` in the TUI changes it until the daemon restarts
  local_workspace_roots: []    # host directories a local workspace may come from
  defaults:
    backend: local             # the backend a new task goes to when it names none
    profile: default           # the profile a new task uses when it names none
  mode: network                # trusted (built-in: loopback, local token) or network (OIDC behind a TLS proxy)
  bind: 0.0.0.0:8080           # what the TLS proxy connects to
  owner_subject: ""
  cors_allowed_origins: []
  oidc:                        # network mode only; trusted mode refuses it
    issuer: https://login.example.com/realms/dev
    audience: narsor-api
  tls:                         # network mode only; trusted mode refuses it
    termination: proxy
    trusted_proxies: [10.0.0.12]
  observer:
    enabled: false             # true starts the observer worker
    server_url: ""
    secrets_dir: ""
    script_user: ""
    insecure_same_user: false
controller:
  local:
    home: host                 # host or private: the HOME of local tasks' tools
    codex_home: ""
    claude_config_dir: ""
    claude_profile_source: ""
    mode: process              # process (built-in: on the host) or docker (in a container; needs the local daemon, HOME is private)
    docker:
      update_interval: 24h     # how long a built task image is used before its component versions are checked again; `none` never checks
runner:
  agent_args:                  # by harness (claude, codex, opencode, copilot); only the running harness's list is added to its command
    claude: [--dangerously-skip-permissions, --effort, high]  # one list entry is one argument, spaces included; win over Narsor's own values of the same flag
  conductor:
    model: medium              # the model tier the conductor runs on (small, medium, large, max); empty is the harness's default
    effort: low                # the effort the conductor runs with (low, medium, high, max); empty is the harness's default
tui:
  refresh_interval: 10s        # how often the TUI re-reads the daemon; `none` turns it off
web:
  bind: 127.0.0.1:8484         # loopback only
  upstream: local              # `local` (the local daemon) or the server's URL
  token: server                # `server` (its own token) or `passthrough` (the browser's)
```

> **Warning — an unknown root is ignored.** A top-level key that is not a
> section (a typo such as `sever:`, or a flat key of the old layout such as
> `server_mode:`) is skipped without an error, and every setting it was meant
> to set keeps its built-in value. Inside a section a program reads, an
> unknown key is an error that names its full path (`server.oidc.isuer`).
> Check the file with `narsor-cli config show`: a value you set that shows
> the source `built-in` did not arrive.

`narsor-server` and `narsor-web` also write a **warning at start** (not an
error) when the file has a top-level key, or the environment has a variable,
with the name of an old flat setting (see
[the table below](#old-and-new-names)). The warning names each old name and
its new one; the program still starts on the values it found.

### The settings

| Path | Variable | Built-in |
|---|---|---|
| `global.root` | `NARSOR_GLOBAL__ROOT` (flag `--root`) | `./.narsor` |
| `global.log_level` | `NARSOR_GLOBAL__LOG_LEVEL` | `off` |
| `server.catalog` | `NARSOR_SERVER__CATALOG` | `.` (the root) |
| `server.state` | `NARSOR_SERVER__STATE` | `.` (the root) |
| `server.work` | `NARSOR_SERVER__WORK` | `.` (the root) |
| `server.agent_harness` | `NARSOR_SERVER__AGENT_HARNESS` | `claude-code` |
| `server.max_concurrent_tasks` | `NARSOR_SERVER__MAX_CONCURRENT_TASKS` | `1` |
| `server.defaults.backend` | `NARSOR_SERVER__DEFAULTS__BACKEND` | `local` |
| `server.defaults.profile` | `NARSOR_SERVER__DEFAULTS__PROFILE` | `default` |
| `server.local_workspace_roots` | `NARSOR_SERVER__LOCAL_WORKSPACE_ROOTS` | empty |
| `server.mode` | `NARSOR_SERVER__MODE` | `trusted` |
| `server.bind` | `NARSOR_SERVER__BIND` | `127.0.0.1:0` |
| `server.owner_subject` | `NARSOR_SERVER__OWNER_SUBJECT` | empty |
| `server.cors_allowed_origins` | `NARSOR_SERVER__CORS_ALLOWED_ORIGINS` | empty |
| `server.oidc.issuer` | `NARSOR_SERVER__OIDC__ISSUER` | empty |
| `server.oidc.audience` | `NARSOR_SERVER__OIDC__AUDIENCE` | empty |
| `server.tls.termination` | `NARSOR_SERVER__TLS__TERMINATION` | `none` |
| `server.tls.trusted_proxies` | `NARSOR_SERVER__TLS__TRUSTED_PROXIES` | empty |
| `server.observer.enabled` | `NARSOR_SERVER__OBSERVER__ENABLED` (`false` or `true`) | `false` |
| `server.observer.server_url` | `NARSOR_SERVER__OBSERVER__SERVER_URL` | empty (the server's own address) |
| `server.observer.secrets_dir` | `NARSOR_SERVER__OBSERVER__SECRETS_DIR` | empty |
| `server.observer.script_user` | `NARSOR_SERVER__OBSERVER__SCRIPT_USER` | empty |
| `server.observer.insecure_same_user` | `NARSOR_SERVER__OBSERVER__INSECURE_SAME_USER` (`false` or `true`) | `false` |
| `controller.local.home` | `NARSOR_CONTROLLER__LOCAL__HOME` | `host` |
| `controller.local.codex_home` | `NARSOR_CONTROLLER__LOCAL__CODEX_HOME` | empty |
| `controller.local.claude_config_dir` | `NARSOR_CONTROLLER__LOCAL__CLAUDE_CONFIG_DIR` | empty |
| `controller.local.claude_profile_source` | `NARSOR_CONTROLLER__LOCAL__CLAUDE_PROFILE_SOURCE` | empty |
| `controller.local.mode` | `NARSOR_CONTROLLER__LOCAL__MODE` (`process` or `docker`) | `process` |
| `controller.local.docker.update_interval` | `NARSOR_CONTROLLER__LOCAL__DOCKER__UPDATE_INTERVAL` | `24h` |
| `runner.agent_args` | `NARSOR_RUNNER__AGENT_ARGS` (a mapping, e.g. `{claude: [--effort, high]}`) | empty |
| `runner.conductor.model` | `NARSOR_RUNNER__CONDUCTOR__MODEL` | empty |
| `runner.conductor.effort` | `NARSOR_RUNNER__CONDUCTOR__EFFORT` | empty |
| `tui.refresh_interval` | `NARSOR_TUI__REFRESH_INTERVAL` | `10s` |
| `web.bind` | `NARSOR_WEB__BIND` | `127.0.0.1:8484` |
| `web.upstream` | `NARSOR_WEB__UPSTREAM` | `local` |
| `web.token` | `NARSOR_WEB__TOKEN` | `server` |

**Environment variables.** The name of a setting's variable is `NARSOR_`
plus the segments of its path in capitals, joined by `__` (two
underscores): `server.oidc.issuer` is `NARSOR_SERVER__OIDC__ISSUER`. A list
is written comma-separated (`NARSOR_SERVER__TLS__TRUSTED_PROXIES=10.0.0.12,10.0.0.13`),
and `runner.agent_args` as YAML text. A program reads only the variables of
its own sections, so a bad `NARSOR_WEB__BIND` does not stop the server. The
`NARSOR_*` variables that are not settings (`NARSOR_SERVER`, `NARSOR_TOKEN`,
`NARSOR_TOKEN_FILE`, `NARSOR_TOKEN_COMMAND`, `NARSOR_CONTROL_PLANE`,
`NARSOR_ENROLLMENT_CODE`, …) are not part of the tree and keep their names.

The `server.mode`, `server.oidc` and `server.tls` settings are described in
[Remote execution backends](../operations/remote-backends.md#network-mode).
In network mode the server needs `server.oidc.issuer`,
`server.oidc.audience`, `server.tls.termination: proxy` and
`server.tls.trusted_proxies`. In trusted mode the server refuses these
settings when they are set, because it would ignore them.

The `web` section is read by `narsor-web`, the server of the
[browser interface](web.md): where it listens (loopback only), the server it
passes the API to (`local` = the local daemon, or the server's URL), and
whose token goes to that server (`server` = its own, `passthrough` = the
browser's). `web.token` is this choice of mode, not a token.

The `server.observer` group tells the server whether to start the observer
worker, which runs the scripts of the catalogue's automations, and how; see
[installation](../operations/installation.md#run-the-observer-worker).
`server.observer.secrets_dir` is one flat directory with one file per secret
name (`{secrets_dir}/{name}`), accessible to its owner only (`0700`). A
script gets only the secrets its automation declares in `automation.yaml`
(see [Automations and queues](observers.md#10-secrets)). The automations
and queues themselves are files of the catalogue (`{catalog}/automations/`,
`{catalog}/queues/`), not settings.

The `controller` section holds the HOME settings and the work mode of
`narsor-controller local`. The controller that a trusted-mode server starts reads the same file
that the server found, and gets only the server's `NARSOR_GLOBAL__*` and
`NARSOR_CONTROLLER__*` variables; the server checks the `controller` section
before it starts and refuses to start on its errors.

`controller.local.mode` sets how a local task's runner runs: `process` (a
process on the host) or `docker` (a container on the local Docker daemon).
The controller reads it when it starts. In Docker mode the HOME of the tools
is always the private HOME on task storage: when `controller.local.home` is
not set, Docker mode uses `private`; an explicitly set
`controller.local.home: host` (or `--home host`) with `mode: docker` stops
the controller and the server. Docker mode needs the local daemon as the
control plane: a controller started by hand for a control plane beyond this
machine refuses to start in Docker mode.
`controller.local.docker.update_interval` sets how long a built task image
is used before the controller checks its component versions again; `none`
never checks again. See [Local tasks in Docker](local-docker.md) for the
task image, the token store and the limits of the Docker mode, and
[Remote execution backends](../operations/remote-backends.md) for a
controller started by hand.

For Kubernetes runners, put `NARSOR_RUNNER__AGENT_ARGS` in the controller
ConfigMap (`narsor-controller-config`). The controller forwards this one key
to the runner as an environment variable; it does not parse or rewrite the
mapping. Each list item remains one argument, including items containing
spaces.

An **empty** variable counts as unset, because that is how Kubernetes renders
an unset key of a ConfigMap.

`COPILOT_CLI_PATH` is not a setting either. It names the GitHub Copilot CLI
that the `github-copilot` harness starts; without it the runner uses
`copilot` on `PATH`. See [Agents](agents.md).

`RUST_LOG` beats `NARSOR_GLOBAL__LOG_LEVEL`: it takes full `tracing`
directives such as `narsor::engine=debug`, where the setting takes a bare
level, and the sharper instrument should not lose. `config show` names
whichever one took effect.

`ORCHESTRATOR_ACTION_ID` exists alongside the `NARSOR_` prefix and is **not** a
setting: it is the idempotency key handed to workflow scripts, so it will not
appear in `config show` and it did not change name.

Secrets are not kept in this file and not handled by it (D16): they come from
the environment, or from a mounted `.env`.

### Directories relative to the root

`global.root` (`--root`, built-in `./.narsor`) is the base directory of the
installation. The three directories of the server are resolved against it:

| `server.state` | Result with `global.root: /srv/narsor` |
|---|---|
| not set (built-in `.`) | `/srv/narsor` |
| `state` | `/srv/narsor/state` |
| `/var/lib/narsor-state` | `/var/lib/narsor-state` |

The same rule applies to `server.catalog` and `server.work`. A relative value
is joined to the root, an absolute value is used as is. `--root` sets
`global.root` only, so `--root /x` with nothing else puts all three
directories at `/x`, as before. The clients resolve `server.state` the same
way to find the local daemon. `config show` shows the joined path; its
source is the source of the directory's own key.

### Old and new names

Before the tree, the file was a flat list of keys and each variable was
`NARSOR_` plus the key in capitals. Those names are **ignored** now: there is
no transition period. See [Updates](../operations/updates.md#update-to-the-hierarchical-configuration-file)
for the upgrade steps.

| New path | Old flat key | Read by |
|---|---|---|
| `global.root` | (new; `--root`) | all |
| `global.log_level` | `log_level` | all (used by `narsor-cli`, `narsor`, `narsor-server`) |
| `server.catalog` | `catalog_dir` | server |
| `server.state` | `state_dir` | server; cli, tui, web |
| `server.work` | `work_dir` | server |
| `server.agent_harness` | `agent_harness` | server |
| `server.max_concurrent_tasks` | `max_concurrent_tasks` | server |
| `server.defaults.backend` | `default_backend` | server |
| `server.defaults.profile` | `default_profile` | server |
| `server.local_workspace_roots` | `local_workspace_roots` | server |
| `server.mode` | `server_mode` | server |
| `server.bind` | `server_bind` | server |
| `server.oidc.issuer` | `oidc_issuer` | server |
| `server.oidc.audience` | `oidc_audience` | server |
| `server.owner_subject` | `owner_subject` | server |
| `server.tls.termination` | `tls_termination` | server |
| `server.tls.trusted_proxies` | `trusted_proxies` | server |
| `server.cors_allowed_origins` | `cors_allowed_origins` | server |
| `server.observer.enabled` | `observer_worker` (`on`/`off`, now `true`/`false`) | server |
| `server.observer.server_url` | `observer_worker_server` | server |
| `server.observer.secrets_dir` | `observer_secrets_dir` | server |
| `server.observer.script_user` | `observer_script_user` | server |
| `server.observer.insecure_same_user` | `observer_insecure_same_user` | server |
| `controller.local.home` | `local_home` | controller |
| `controller.local.codex_home` | `local_codex_home` | controller |
| `controller.local.claude_config_dir` | `local_claude_config_dir` | controller |
| `controller.local.claude_profile_source` | `local_claude_profile_source` | controller |
| `runner.agent_args` | `agent_args` | runner |
| `runner.conductor.model` | `conductor_model` | runner |
| `runner.conductor.effort` | `conductor_effort` | runner |
| `tui.refresh_interval` | `tui_refresh_interval` | tui, cli |
| `web.bind` | `web_bind` | web |
| `web.upstream` | `web_upstream` | web |
| `web.token` | `web_token` | web |
| (removed) | `agent_timeout`, `escalation_threshold` | nobody |

The old variable of each key was `NARSOR_` plus the key in capitals
(`server_mode` was `NARSOR_SERVER_MODE`, now `NARSOR_SERVER__MODE`; the
runner's `NARSOR_AGENT_ARGS` is now `NARSOR_RUNNER__AGENT_ARGS`).
`agent_timeout` and `escalation_threshold` were removed, not moved: nothing
read them.

### Seeing what is in force

```bash
narsor-cli config show                  # a table: setting, value, source, programs
narsor-cli config show --format json    # the same, for a script
```

`config show` lists every setting of every program installed next to
`narsor-cli`. `narsor-cli` checks its own sections itself, and runs every
other program (`narsor-server`, `narsor-web`, `narsor`, `narsor-runner`,
`narsor-controller`) in its **check mode**: the program resolves its own
sections from the same file (`--config` is passed on, and `--root` to the
programs that have it), runs its own rules, prints the result and exits
without starting anything. The table merges the results, grouped by section;
a setting that several programs read with the same value is one row naming
them all. Below the table, each program has a status: `ok`, `errors: N
problem(s)`, `not installed` (not an error) or `failed to run`.

```
SETTING                       VALUE                    SOURCE    READ BY
[global]
global.root                   /srv/narsor              file      narsor-cli, narsor-server, narsor-web, narsor, narsor-runner, narsor-controller
global.log_level              info                     file      narsor-cli, narsor-server, narsor-web, narsor, narsor-runner, narsor-controller
[server]
server.catalog                /srv/narsor              built-in  narsor-server
server.state                  /srv/narsor/state        file      narsor-cli, narsor-server, narsor-web, narsor
…
server.mode                   network                  file      narsor-server
server.oidc.issuer            https://idp.example.com  file      narsor-server
server.oidc.audience                                   built-in  narsor-server
                              error: server.mode is 'network' but server.oidc.audience is not set; …
…
[web]
web.bind                      (error)                  -         narsor-web
                              error: Configuration file '/home/you/.config/narsor/config.yaml', key 'web.bind': 'bad' is not host:port (for example 127.0.0.1:7400)
web.upstream                  local                    built-in  narsor-web
web.token                     server                   built-in  narsor-web

file: /home/you/.config/narsor/config.yaml
shadowed: /etc/narsor/config.yaml

BINARY             STATUS
narsor-cli         ok
narsor-server      errors: 3 problem(s)
narsor-web         errors: 1 problem(s)
narsor             ok
narsor-runner      ok
narsor-controller  not installed
```

Every setting is listed, including the ones nobody touched: the question is
what is in force, not what was changed. The JSON output (`version: 2`) has the
settings as a list of rows (`path`, `value`, `source`, `error`, `sensitive`,
`binaries`), the errors that mark no row, and the status of each program.

Each program also accepts the hidden flag `--check-config` and prints its own
check as one JSON document; this is what `config show` runs. It exits 0 when
its sections hold up and 1 when they do not, and starts nothing:

```bash
narsor-server --config /etc/narsor/config.yaml --check-config
```

### When the file does not hold up

Each program checks `global` and its own sections when it starts: values out
of range, an unknown key inside one of its sections, a section or a group
written as a single value, and the rules between settings (network mode
needs `server.oidc.*` and `server.tls.*`, and trusted mode refuses them; an
enabled observer worker needs exactly one of `server.observer.script_user`
and `server.observer.insecure_same_user`, and in network mode
`server.observer.server_url`). A file that exists but is unreadable or
malformed, or a problem in the program's own sections, stops that program
with exit code 1, with every fault reported at once with its full path. The
server runs these checks before it binds its API.

`config show` is the single exception. It still prints the whole table, marks
the keys that did not hold up, and exits 1 — a diagnostic tool that stops
working in the situation it exists for is no tool at all. The exception does
not extend to a `--config` path that is not there: that is a usage error (exit
code 2) for every command including this one.

### Coming from an older checkout

The application used to be called `orchestrator`, and the rename was a clean
cut: the old paths are not read as a fallback. If you have data or workflows
under the old names, move them once:

```bash
mv ./.orchestrator ./.narsor
mv ~/.config/orchestrator ~/.config/narsor
```

### What is a contract

The `NARSOR_` prefix, the cascade paths, the `config show` command and the
shape of its JSON are treated as a contract. The JSON carries a `version`
number so that a later change of shape is a recognisable change rather than a
breakage. The move to the tree of sections broke the names of the settings and
their variables on purpose, and raised the JSON to `version: 2`; see
[Updates](../operations/updates.md#update-to-the-hierarchical-configuration-file).

---

## The on-disk layout

With the built-in directories, everything is under the root (`global.root`,
`--root`, `./.narsor` by default). `server.state`, `server.catalog` and
`server.work` move their parts elsewhere (see
[Directories relative to the root](#directories-relative-to-the-root)):

```text
{root}/
├── repositories.json               # the repository registry (`repo add` …)
├── executions/{execution_id}/      # executions created before the task scheduler
│   ├── state.json                  # WorkflowExecution: status, current state, context
│   ├── context.json                # the context projection (variables, artifacts, workspace)
│   ├── pending_input.json          # the question an input-needed execution is waiting on
│   ├── definition.yaml             # pinned copy of the workflow definition for this execution
│   ├── checkpoint.<repo>.bundle    # per workflow repository: git bundle of its delta against base_commit
│   ├── checkpoint.<repo>.git/      # scratch area for materialising that checkpoint
│   ├── events/{sequence:08}.json   # append-only event log
│   ├── conversations/{action_execution_id}.jsonl(.zst)  # per-attempt agent conversation record
│   └── artifacts/{artifact_id}/    # files produced by actions
├── orchestration/                  # durable queued tasks, their order (rank), leases
├── local-controller/credential     # the daemon-owned local controller's identity
├── local-controller/secrets/       # the token store of the Docker mode (`secret set`), 0700
├── local-controller/images/        # the Docker mode's task image cache and refresh marker
├── local-controller/storage/{task_id}/   # under `server.work` (the root by default)
│   ├── state/                      # execution history once the runner initializes it
│   └── workspace/                  # provisioned only after scheduler admission
├── local-controller/tasks/{task_id}.json # under `server.work`: last runner kind and image of a task
└── narsor.log                # only written by `tui`, and only with RUST_LOG set
```

Every write of `state.json`, `context.json` and `repositories.json` is atomic
(temporary file in the same directory, `fsync`, `rename`), and event numbering
increases monotonically without gaps. An interrupted write leaves the previous
version of the file intact.
