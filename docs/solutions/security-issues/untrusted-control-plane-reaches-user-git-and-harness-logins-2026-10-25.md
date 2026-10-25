---
title: A private HOME did not keep the user's git and harness logins from an untrusted control plane
date: 2026-10-25
category: security-issues
module: narsor-controller local
problem_type: security_issue
component: authentication
symptoms:
  - "A control plane beyond loopback could send a Git workspace with a `file://`, `ssh://`, `git@host:` or plain-path remote and get a clone of a repository on the user's disk, or one their SSH keys open"
  - "With `--home private` the runner still cloned with the user's `~/.gitconfig`, credential helpers, `url.*.insteadOf` rewrites and `SSH_AUTH_SOCK`, because git read them from the inherited environment"
  - "The runner inherited `GH_TOKEN`, `GITLAB_TOKEN`, `CLAUDE_CODE_OAUTH_TOKEN`, `CODEX_HOME`, `CLAUDE_CONFIG_DIR` and `GIT_CONFIG_*` from the user's shell"
  - "An https remote could name `localhost`, `169.254.169.254` or a private address, so the server could probe the user's network through git's errors"
root_cause: missing_validation
resolution_type: code_fix
severity: critical
tags: [local-controller, untrusted-control-plane, git-credentials, harness-login, private-home, ssrf, environment-scrubbing, r17]
related_components: [narsor-engine workspace provisioner, narsor-engine local backend, narsor-runner]
---

# A private HOME did not keep the user's git and harness logins from an untrusted control plane

## Problem
`narsor-controller local` can serve a control plane beyond this machine (R16–R18, D68). For that case the design treats the server as untrusted and gives tasks a private HOME. But a private HOME alone did not keep the user's logins away from the tasks. Git and the harnesses find credentials through many paths that are not under HOME, and the Git workspace remote was not checked at all.

## Symptoms
- The launch check (`crates/narsor-controller/src/local/untrusted.rs`) checked only `TaskWorkspace::Local` directories. A `TaskWorkspace::Git` remote went to `git clone` without a check.
- The runner changed `HOME`, but git also reads `GIT_CONFIG_GLOBAL`, `GIT_CONFIG_PARAMETERS`, `GIT_CONFIG_COUNT`/`KEY_n`/`VALUE_n`, `XDG_CONFIG_HOME`, the system configuration, `SSH_AUTH_SOCK`, `GIT_ASKPASS`, `SSH_ASKPASS` and `GIT_SSH(_COMMAND)`.
- The runner process inherited forge and harness tokens from the shell that started the controller.
- `--codex-home`, `--claude-config-dir`, `--claude-profile-source` (or their configuration-file values) gave every remote task the user's own harness login, without a warning.

## What Didn't Work
- **Rely on the private HOME (the state before the fix).** HOME hides only `~/.gitconfig`, `~/.ssh` and the harness files under HOME. Variables and the system configuration still give git and the harnesses the user's credentials.
- **Check the remote with a URL parser only.** Git reads the text, not the parser's view. `https:host/x` is an SSH remote to git, but `Url::parse` sees `https://host/x`. Also `HTTPS://`, leading whitespace and `\` can make the parser and git disagree. The check now asks for the literal prefix `https://` and no whitespace, control characters or `\` before it parses the URL.
- **Resolve the host name to find private addresses.** The check does not resolve DNS (that is a time-of-check/time-of-use gap and a probe by itself). It refuses only literal internal addresses and `localhost`/`*.localhost`. A public host that redirects to a private address is stopped differently: git follows no redirect (see below).

## Solution
All in commit `23db582`. The restrictions apply only when the control plane is `Remote` (beyond loopback) and the HOME mode is `private`. `--accept-host-home` turns them off, and the controller then prints a warning for each harness directory it gives to tasks.

1. **The launch check refuses a Git remote that is not plain `https://` to an external host** (`check_remote` and `is_internal` in `untrusted.rs`). Internal means: loopback, RFC 1918, link-local (including `169.254.169.254`), unspecified, broadcast, RFC 6598 (`100.64.0.0/10`), IPv6 ULA `fc00::/7`, IPv6 link-local `fe80::/10`, IPv4-mapped IPv6 of these, and `localhost`/`*.localhost` (also with a final dot). Integer hosts such as `https://2130706433/` are normalized by the URL parser and refused. A refusal message removes the user name and password from the remote (`without_credentials`).

2. **The runner clones with none of the user's git setup** (`GitAccess::PinnedOnly` and `restrict_git_access` in `crates/narsor-engine/src/workspace/provisioner.rs`, runner flag `--pinned-git-credential-only`):

   ```rust
   for name in USER_GIT_ACCESS_ENVS {   // SSH_AUTH_SOCK, *_ASKPASS, GIT_SSH*, GIT_CONFIG*, XDG_CONFIG_HOME
       command.env_remove(name);
   }
   command
       .env("HOME", home)
       .env("GIT_CONFIG_GLOBAL", "/dev/null")
       .env("GIT_CONFIG_NOSYSTEM", "1")
       .env("GIT_CONFIG_COUNT", "1")
       .env("GIT_CONFIG_KEY_0", "http.followRedirects")
       .env("GIT_CONFIG_VALUE_0", "false")
       .env("GIT_ALLOW_PROTOCOL", "https");
   ```

   The pinned credential of the task is the only one git has. `http.followRedirects=false` stops a public host from sending the clone on to a private network. `GIT_ALLOW_PROTOCOL=https` is a second guard behind the launch check (it also blocks submodules over other transports).

3. **The local backend does not give the runner the user's credential variables** (`withhold_user_credentials` and `is_user_credential` in `crates/narsor-engine/src/backends/local.rs`). It removes every `GIT_CONFIG*` variable, the git list above, and `CODEX_HOME`, `CLAUDE_CONFIG_DIR`, `CLAUDE_CODE_OAUTH_TOKEN`, `ANTHROPIC_AUTH_TOKEN`, `GH_TOKEN`, `GITHUB_TOKEN`, `GH_ENTERPRISE_TOKEN`, `GITHUB_ENTERPRISE_TOKEN`, `GH_CONFIG_DIR`, `GITLAB_TOKEN`, `GL_TOKEN`, `GLAB_TOKEN`, `GLAB_CONFIG_DIR`. This is in addition to the `NARSOR_*` variables, which a runner never inherits.

4. **Harness directories need consent** (`resolve_home` in `crates/narsor-controller/src/local.rs`). With a remote control plane and no `--accept-host-home`, a harness directory flag is a usage error. The harness directories from the configuration file are ignored.

The same commit also changed these related items: enrollment of any `local` backend needs the workstation risk accepted, whatever its trust scope; the controller lock is now `<credential file>.lock`, so two controllers whose credentials are in one directory do not wait for each other; runner HTTP clients follow no redirect.

## Why This Works
The danger is that a remote server picks what the user's machine does with the user's identity. HOME is only one of the paths to that identity. The fix closes each path at the point where it is used:

- the launch check decides *what* may be cloned (only external https);
- `GitAccess::PinnedOnly` decides *with what* git clones (only the pinned credential, no redirect);
- the environment scrub decides *what the runner and the harness can find* (no token from the shell);
- the consent flag makes the one remaining open path (the user's own harness directories) explicit.

The checks are layered, so one gap (for example, a new git variable) does not open all paths.

## Prevention
- When a task runs for an untrusted party on the user's machine, list every path through which a tool finds credentials: files under HOME, variables, the system configuration, agents (SSH, GPG), helpers, and network redirects. A private HOME covers only the first.
- Check remote URLs against the text that the consuming tool reads, not only against a parser. Keep tests with parser-differential inputs (`https:host/x`, `HTTPS://`, leading space, `\@`).
- Keep the regression tests in place:
  - `a_git_workspace_is_cloned_over_https_only`, `a_git_workspace_on_this_machine_or_its_network_is_refused`, `a_refusal_does_not_show_the_remotes_credentials` (`untrusted.rs`);
  - `a_pinned_only_git_uses_none_of_the_users_own_setup` (`provisioner.rs`) — it runs real `git config` with a fake user setup and checks that no helper is visible;
  - `the_users_logins_are_credential_variables` (`backends/local.rs`).
- When the runner or a harness starts to support a new forge or harness, add its token and configuration variables to `USER_CREDENTIAL_ENVS`.
- This is a security control. A security specialist must review the list of variables and internal address ranges before a release that lets a control plane beyond loopback use a workstation.

## Related Issues
- `docs/solutions/integration-issues/runner-private-home-hides-harness-login-2026-10-10.md` — the other side of the same setting: for a trusted local daemon the default is `host`, so that harness logins work (D57). This doc covers why `private` alone is not sufficient for an untrusted server.
- `ARCHITECTURE_DECISIONS.md` D57 (HOME mode), D68 (local controller as a separate process).
- `docs/plans/2026-10-25-004-refactor-lokalny-controller-osobny-proces-plan.md` — requirements R16–R18.
