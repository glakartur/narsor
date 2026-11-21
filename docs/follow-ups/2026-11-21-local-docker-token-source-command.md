# Follow-up: token source as a command for the local Docker work mode

Status: open. No plan and no decision to implement yet.
Origin: `docs/plans/2026-11-21-001-feat-local-docker-work-mode-plan.md`
(KTD10) and decision D81 in `docs/dev/architecture.md`.

## Problem

`narsor-cli secret set <name>` stores each token as a plain-text file in the
controller's token store (`<server.state>/local-controller/secrets/<name>`,
directory 0700, file 0600). This is the same protection level as the logins
of Claude Code, `gh` and `glab`. It protects against other local users and
against the agent in the container. It does not protect against:

- a program that runs as the same user (malware, a malicious dependency, an
  agent in process mode);
- home-directory backups, which contain the token in plain text;
- theft of an unencrypted disk.

The token from `claude setup-token` is valid for one year, so a leak has a
long effect.

## Proposal

Let the operator name, per secret, a host command that prints the token,
instead of storing the value in the token store. Examples:

- `secret-tool lookup service narsor name claude-token` (Secret Service /
  GNOME Keyring / KWallet);
- `pass show narsor/gitlab-token`;
- `op read op://narsor/gitlab/token` (1Password CLI).

The controller runs the command when it prepares a container start and writes
the output only into the per-workload secrets directory
(`<workload dir>/secrets`, 0700/0600), which is removed with the workload.
The token then has no long-lived plain-text copy on disk.

## Points to decide

- Where the command is defined: in the controller-owned store (for example a
  `<name>.command` file set by `narsor-cli secret set <name> --command …`) or
  in the controller configuration section. The catalogue is a poor fit: it is
  shared with the control plane, and a command there would run host code
  from catalogue data.
- Execution rules: no shell interpolation, scrubbed environment (no
  `NARSOR_*`, no `API_KEY_ENVS`), a timeout, an output size cap, trim of one
  trailing newline; a failure is a readable provisioning problem that names
  the secret and the command (never its output).
- Behavior when the keyring is locked (no unlocked desktop session, for
  example after a reboot on a headless machine): a transient problem with a
  retry, or a permanent one with a clear message.
- Whether a stored value and a command can both exist for one name, and
  which one wins.
- Interaction with the remote control plane: Docker mode is refused there
  today (KTD13), so the command never runs on behalf of an untrusted server.
  Keep it that way.

## Related hardening (independent)

- Document full-disk encryption (LUKS) as the recommended base protection.
- Recommend scoped, expiring tokens (GitLab PAT with the minimum scope and an
  expiry date).
- `narsor-cli secret list` / `remove` (deferred in the plan) to let the
  operator see and revoke stored tokens.
