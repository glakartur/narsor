# Follow-up: per-workflow secrets for the local Docker work mode

Status: open. No plan and no decision to implement yet.
Origin: image-level `task_image.secrets` in `runtime.yaml` and decision D81
in `docs/dev/architecture.md` (KTD10, tokens).

## Problem

Secrets of the local Docker work mode come from the controller's token store
and are declared in `runtime.yaml` in two places:

- `task_image.components[].secrets` — the secrets a component's tool needs;
- `task_image.secrets` — secrets that no component owns (for example a
  database password for integration tests).

Both are sealed into every local task and the controller copies them into
every Docker-mode container. All workflows and all steps of each task
receive every value, agent and script steps alike.

An operator often needs a value only in some places: a database password
only for the workflow that runs integration tests, or only for the script
step that runs them, not for the agent step that writes code. Today the only
way to limit the exposure is to keep the secret out of the catalogue, which
removes it from every task.

## Options

1. **Secrets in the workflow definition.** `globals.secrets` (whole
   workflow) and a state-level `secrets` (one step), each entry
   `{ name, env | file }` that references a token store name. The definition
   already reaches the controller in the launch spec, so the controller can
   copy only what the task needs and the runner can deliver per step.
2. **Profile-level secrets for the local backend.** User profiles already
   carry `secrets` (names from the backend's allow list). Let the local
   backend resolve them from its token store in Docker mode. The scope is
   the user and repository, not the workflow.
3. **Per-step environment scoping in the runner.** Keep the declaration in
   `runtime.yaml` (or option 1), but let the runner give an `env` secret
   only to the steps that ask for it, instead of to the whole task
   environment.

The options can be combined: option 1 for declaration, option 3 for
delivery.

## Points to decide

- **Sealing and identity.** Whether workflow-level secret references are
  sealed into the task (and so enter the environment identity) like
  `task_image`, or are read from the snapshotted definition. A change of the
  definition must not change the secrets of a running task without a new
  version.
- **When names are checked.** Validation that every name exists in the token
  store at task creation (the control plane does not see the controller's
  store) or at provision (today's behavior: a missing entry refuses the
  task with `narsor-cli secret set <name>`). A late failure is worse for the
  user; an early check needs a new controller query.
- **Process mode.** Whether a process-mode task receives the secrets too
  (from the same store), ignores them, or is refused. Today the token store
  serves only Docker mode.
- **Conductor and agent exposure.** Whether the conductor (a host process)
  and agent steps see secrets declared for a workflow, and how a step-scoped
  secret is kept out of the agent's environment and transcript.
- **Least privilege per step.** A step-level declaration lets a script step
  receive a value that the agent step never sees. Decide whether this is the
  default (secrets only where declared) and how `file` delivery works per
  step when the private HOME is shared by all steps of a task.
- **Precedence and duplicates.** How workflow-level entries combine with
  `task_image.secrets` and component secrets: today two entries that deliver
  the same environment variable or file are refused.

## Related items

- `docs/follow-ups/2026-11-21-local-docker-token-source-command.md` — a
  token source as a host command instead of a plain-text store entry.
- `narsor-cli secret list` / `remove` (deferred in the Docker work mode
  plan) to see and revoke stored tokens.
- `docs/user/local-docker.md`, section "Secrets for every task".
