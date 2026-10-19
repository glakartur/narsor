---
title: "Workspace checkout used a hard-coded main: fall back to the registry default_branch through one shared rule"
date: 2026-10-19
category: logic-errors
module: engine/workspace
problem_type: logic_error
component: tooling
symptoms:
  - "Task creation fails with `git ref 'main' not found in repository` for a repository whose default branch is not main"
  - "The bundled workflows pinned `ref: main`, so the `default_branch` of the repository configuration was never used"
  - "The ref error did not say whether the name came from the workflow or from the repository configuration"
root_cause: logic_error
resolution_type: code_fix
severity: medium
tags: [workspace, git-ref, default-branch, registry, workflow-ref, ref-origin, resolver]
---

# Workspace checkout used a hard-coded main

## Problem

A workspace checkout took its branch from the workflow `ref`. The bundled workflows pinned `ref: main`. A repository with another default branch could not be checked out. The `default_branch` of the registry entry did not help, because the workflow `ref` always won.

## What Didn't Work

The first fix copied the "workflow ref, else `default_branch`" choice into two places: the launch resolver and `resolve_repository` in `tasks`. The two copies differed. One trimmed a blank `default_branch` and the other did not. (from git history, commits 9f0dd27 and a3d88a2)

## Solution

1. Remove `ref: main` from the bundled workflows. Without a workflow `ref`, the registry `default_branch` applies. With neither, the clone takes the remote `HEAD`.
2. Keep the choice in one method, `RepositoryEntry::named_ref(workflow_ref)`. It returns the ref and a `RefOrigin` (`Workflow` or `Repository`). An explicit workflow ref wins. A blank `default_branch` counts as unset.
3. Carry `ref_origin` through `RepositorySpec` and `RepositorySource::Git` to the provisioner. `WorkspaceError::RefNotFound` now says where the name came from.

```rust
pub fn named_ref(&self, workflow_ref: Option<&str>) -> Option<(String, RefOrigin)> {
    match workflow_ref {
        Some(git_ref) => Some((git_ref.to_string(), RefOrigin::Workflow)),
        None => self.default_branch.as_deref().map(str::trim)
            .filter(|branch| !branch.is_empty())
            .map(|branch| (branch.to_string(), RefOrigin::Repository)),
    }
}
```

## Why This Works

The repository configuration is the only place that knows the real default branch. A workflow is shared across repositories, so it must not name a branch unless it needs one. One shared rule keeps the resolver and the task code from drifting apart. The origin in the error shows which setting to fix.

## Prevention

- Do not pin a branch in bundled workflows. Leave `ref` empty unless the workflow needs a specific ref.
- Add a rule that applies in two code paths to one function, and call it from both.
- Keep tests for: explicit workflow ref wins, `default_branch` is used, blank `default_branch` is ignored, and the missing-branch message names its origin (`tests/builtin_workflows.rs`, `tests/environment.rs`, `tests/tasks.rs`).
