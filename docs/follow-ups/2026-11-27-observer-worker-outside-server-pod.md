# Follow-up: run the observer worker outside the server Pod

Status: open. No plan and no decision to implement yet.
Origin: brainstorm of 2026-11-27 (automation scheduler debugging session).
Related: `docs/plans/2026-11-13-002-feat-obserwatory-zrodel-zewnetrznych-plan.md`
(line 80 "Etap 2", line 83 D19/Q19 isolation, D5 worker key).

## Problem

`narsor-server` starts `narsor-observer-worker` as a child process
(`docs/dev/components.md`, `docs/operations/installation.md` "Run the
observer worker"). On Kubernetes the worker, and with it every `check`,
`in` and `out` script of the catalogue, runs in the server Pod. Isolation
is only a separate `script_user` (`no_new_privs`, run directory `0700`).

- **Security.** Scripts read untrusted content (Jira, GitLab). A script
  that starts an AI agent with shell access opens a prompt-injection path
  next to the control plane: the same Pod network, file system and kernel.
- **Tools.** The tools a script can use are the tools of the server image.
  For runners the workflow image controls the tools; for automations
  nothing does.
- **Resources.** A heavy script takes CPU and memory from the server.

Scope: a platform requirement (flexible and secure for any automation,
AI included), not a fix for one known automation.

## Constraint from the owner

No Pod per run and no Pod per automation. At most **one separate Pod for
all automations**.

## Proposed direction (minimal)

1. **Worker as its own Deployment on Kubernetes.** A sample manifest in
   `deploy/kubernetes/`, managed by the operator (like
   `deploy/kubernetes/controller.yaml`). Kubernetes restarts it; no new
   workload kind in the controller.
2. **Tools come from the worker image.** The operator builds the image from
   the Narsor worker base plus the tools the automations need (`gh`, `jq`,
   `claude`, …) and pins it by digest. `narsor-cli` is part of the base and
   is always on `PATH`.
3. **Worker credential.** The worker no longer gets a key from a parent
   process. The server issues a long-lived, revocable credential with the
   same scope as today's key (claim and result only) and keeps only its
   digest. The operator puts it in the worker's Secret. **This reverses D5**
   ("no `observer worker-token` command") and must be recorded as a
   decision.
4. **Secrets.** Reuse the documented pattern (`remote-backends.md` §4): one
   Secret, an init container copies the values to a `0700` directory on an
   in-memory `emptyDir`, `server.observer.secrets_dir` points to it. A run
   still gets only the secrets its automation declares.
5. **Sample NetworkPolicy** for the worker Pod: egress only to the control
   plane (`server.observer.server_url`) and to an operator list of hosts.
   Today Narsor does not isolate egress at all (`remote-backends.md`).
6. **Local installs unchanged.** Without Kubernetes the worker stays a
   child process of the server.

Unchanged: schedule, backoff, rounds, hooks and claim stay in the server.
Scripts keep `NARSOR_SERVER`, `NARSOR_TOKEN` (run token, effect routes of
that run only, `check` runs only) and `NARSOR_OBSERVER_RUN`.

## Residual risk (accepted for now)

- All automations share one Pod and one `script_user`. Two concurrent runs
  can interfere through files or processes of the same uid.
- An AI agent after prompt injection sees the worker Pod, the secrets of
  its own automation and its run token. It does not see the server state,
  backend secrets or other Pods. The worker credential is safe from the
  script: a different uid cannot read `/proc/<pid>/environ` of the worker.
- A rollout of the worker image loses the run in progress; the existing
  lost-run semantics apply (no failure count).

## Points to decide

- Credential bootstrap: CLI-issued long-lived credential (proposed) or a
  one-time enrollment code exchanged for a credential, as for the
  controller. Enrollment needs a state volume for the worker; the security
  level is the same, because the credential lives in the worker Pod either
  way.
- Whether the server refuses the embedded worker in network mode, or only
  the documentation recommends the separate Deployment.
- How a revoked or rotated credential is handled while the worker runs.

## Rejected: over-engineering for the current goal

Each item works, but adds code or moving parts that the security goal does
not need. Keep them for when a concrete need appears.

- **Controller-managed worker Pod** (`narsor-controller kubernetes` creates
  and recreates the Pod). No RBAC change is needed (the controller already
  has `pods: create, delete, watch` and `secrets: create`), but it adds a
  new non-task workload kind, its reconcile loop and an admission-policy
  exception (task Pods require `restartPolicy: Never`; `allowedSecrets` must
  cover observer secrets). A Deployment gives the same isolation for free.
  Revisit if operators must not maintain a manifest.
- **Tools declared per automation, image composed from the runtime
  catalogue** (`automation.yaml: tools: [gh, claude]`; the worker image is
  the union, built like the runner from the bundle and tool packages under
  `/opt/narsor-tools`; a catalogue change rolls the Pod). Gives catalogue
  control of tools without an image build, but it is a new mechanism; an
  operator-built image gives the same control. Revisit if many teams edit
  automations and image builds become a bottleneck.
- **Per-run `PATH` from declared tools.** Keeps declarations honest (an
  undeclared tool gives `command not found`). It is hygiene, not a
  security boundary: a script can call a tool by absolute path. Depends on
  the item above.
- **Worker key through the controller's launch Secret** (the server issues
  the key through the controller protocol, the key changes on every Pod
  start, D5 semantics kept). Needs a new path in the controller protocol;
  only useful together with the controller-managed Pod.
- **Per-run uid** (a fresh uid from a range per run) to stop interference
  between concurrent runs. Revisit if the worker starts to run several
  runs at the same time or hosts automations of different trust levels.

## Rejected: other directions

- **Ephemeral backend workload per run or per round** (Pod on Kubernetes,
  process or container locally; one isolation model for tasks and
  automations, image per automation). Rejected by the owner: no Pod per run
  or per automation.
- **Automation as a workflow** (a tick creates a task; `check` is a step in
  a runner Pod; AI is a normal harness step). Reuses the runner, but every
  tick adds a task record, history and latency, and rounds and `in`/`out`
  hooks need remapping. Today a `check` script can already create a task
  for agent work; that remains the way to run AI with full task isolation.
- **Execution classes** (trusted shell stays in the server; only
  automations that declare an image or AI go to an isolated workload). Two
  paths to maintain, and security depends on correct classification; the
  server Pod still runs scripts.
- **Kubernetes CronJob** (plan 2026-11-13-002, "Etap 2"). Duplicates the
  schedule, backoff and rounds that the server computes, and cannot serve
  event-driven `in`/`out` hooks.
