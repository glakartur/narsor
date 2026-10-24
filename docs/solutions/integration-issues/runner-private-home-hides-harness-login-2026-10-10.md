---
title: The runner's private HOME hid the harness subscription login from local tasks
date: 2026-10-10
category: integration-issues
module: runner
problem_type: integration_issue
component: authentication
symptoms:
  - "codex and Claude Code inside a local task answer `Not logged in`, although both are logged in on the console"
  - Local tasks broke once they started going through `narsor runner` (b9ef026), with no change to the harnesses
  - An inherited ANTHROPIC_API_KEY / OPENAI_API_KEY silently moves a harness from the subscription to API billing
root_cause: config_error
resolution_type: code_fix
severity: high
tags: [runner, home, harness-login, codex, claude-code, subscription, api-key, local-backend]
---

# The runner's private HOME hid the harness subscription login from local tasks

## Problem
Once local tasks started going through `narsor runner`, the runner pointed `HOME` at a private directory on task storage (R17). The harness CLIs keep their login under HOME, so every agent action in a local task ran with no login.

## Symptoms
- codex and Claude Code answer `Not logged in` inside the task, while the same binaries on the console work.
- `codex` looks for `~/.codex/auth.json` and Claude Code for `~/.claude/.credentials.json`. Under the private HOME neither file exists.
- A related hazard: an `ANTHROPIC_API_KEY`, `OPENAI_API_KEY` or `CODEX_API_KEY` inherited from the daemon's environment makes the harness bill the API instead of the subscription, without any warning.

## What Didn't Work
- **Keeping the private HOME and symlinking the login files into it (rejected in D57).** CLIs save files atomically (write a temp file, then rename), so the first save replaces the link with a copy. When refresh tokens rotate, the copies invalidate each other.
- **Pinning the HOME mode to the task at creation.** The mode should not change during a task's life, but it is a property of where the task runs, not of the task. A pinned value would also have to enter `ResolvedEnvironment` and change the environment-identity hashes.

## Solution
The HOME mode became a **launch setting of the backend**, passed to the runner on every start (commit `a2c5b16`, decision D57):

- New flags `narsor runner --home private|host [--codex-home DIR] [--claude-config-dir DIR]`, modelled by `HarnessHome` / `HomeMode` in `src/runner/home.rs`.
- The local backend defaults to `host` through the settings `local_home` (default `host`), `local_codex_home` and `local_claude_config_dir`.
- Kubernetes keeps `private` unless `harness_home` in `backend.yaml` says otherwise.
- `--codex-home` and `--claude-config-dir` are exported as `CODEX_HOME` and `CLAUDE_CONFIG_DIR`.

`ProcessEnvironment.home` became `Option<PathBuf>`, where `None` means "keep the HOME the runner was started with":

```rust
// src/runner/secrets.rs
pub fn variables(&self) -> impl Iterator<Item = (&str, &str)> {
    self.home
        .iter()
        .map(|home| ("HOME", home.to_str().unwrap_or_default()))
        .chain(/* task variables, then env secrets */)
}
```

The runner always drops the inherited API keys, unless the task was given that key as an `env` secret:

```rust
pub fn removed(&self) -> impl Iterator<Item = &'static str> + '_ {
    API_KEY_ENVS
        .into_iter()
        .filter(|name| self.secrets.iter().all(|(secret, _)| secret != name))
}
```

A `file`-delivered secret is refused when there is no private HOME. Otherwise its symlink would be written into the operator's real HOME.

## Why This Works
Logging in is part of the machine a task runs on, not part of the task. With `host`, a local task sees the same `~/.codex` and `~/.claude` as the console, which is how local tasks ran before the runner existed. Reading the mode on every start keeps it out of the task's identity. An operator can switch it, at the cost of leaving behind the sessions saved under the old mode. Dropping the API-key variables makes subscription billing the only default; using a key becomes an explicit per-task choice made through a secret.

## Prevention
- When a process boundary changes `HOME`, `XDG_*` or `*_CONFIG_DIR`, list which tools keep credentials there before changing it. Harness CLIs do, and so do `gh`, `glab` and `git`'s credential helpers.
- Any change that routes local work through a different launcher (here `b9ef026`) needs an end-to-end smoke run of a real harness on a subscription login. Unit tests with fake harnesses cannot catch a missing login.
- Keep the tests that pin this down: `a_host_home_leaves_home_alone` in `src/runner/secrets.rs`, the `HarnessHome::runner_args` tests in `src/runner/home.rs`, the HOME cases in `crates/narsor-runner/src/runner/mod.rs` and `runner/cli.rs` (unit tests), and `tests/config_resolve.rs`.
- Never pass API-key variables through from the parent environment. A key in the daemon's shell must not decide how a task is billed.

## Related Issues
- `ARCHITECTURE_DECISIONS.md` D57 (this decision) and R17 (private HOME, now Kubernetes-only by default).
- `docs/operations/remote-backends.md`, section `harness_home`.
- Still open: codex on a subscription inside Kubernetes needs a writable `auth.json` that it refreshes, which read-only secrets (R19) do not provide. For Claude Code, `claude setup-token` delivered as `!env CLAUDE_CODE_OAUTH_TOKEN` works.
