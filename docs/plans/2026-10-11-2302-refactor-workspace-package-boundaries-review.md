---
title: "Review: rozdzielenie odpowiedzialności w Cargo workspace"
date: 2026-10-11
reviewed_artifact: docs/plans/2026-10-11-2302-refactor-workspace-package-boundaries-plan.md
review_status: complete_with_peer_failure
---

# Resolved document review

## Artifact identity and source

- Reviewed artifact: `docs/plans/2026-10-11-2302-refactor-workspace-package-boundaries-plan.md`
- Product contract origin: `docs/brainstorms/2026-09-26-narsor-daemon-lokalny-requirements.md`
- Review type: `unified-plan`
- Compound Packs: none declared (`packs-resolve.py` returned no roots, warnings, or errors).

## Reviewer coverage

| Reviewer | Result | Disposition |
|---|---|---|
| coherence | completed | No retained findings. |
| feasibility | completed | One verified package-boundary omission corrected in the plan. |
| security-lens | completed | Discovery-file protection requirement made explicit based on the origin and current implementation. |
| product-lens | completed | No retained findings after full origin-aware review; local co-installation contract was added during synthesis. |
| scope-guardian | completed | One finding dismissed because the dedicated runner package is a user-settled decision. |
| adversarial | completed | No retained findings. |
| cross-model peers and whole-document sweep | incomplete | Four jobs ended without results in the network-disabled command sandbox; no peer findings were used. |

## Findings and dispositions

### Feasibility: controller helper crossed a package boundary

- Evidence: `src/daemon/runtime.rs` calls `crate::cli::controller::load_or_enroll`; the server also starts an in-process local reconciler, while the CLI controller command runs a Kubernetes controller.
- Consequence: moving the controller command wholesale would either leave server code dependent on the CLI package or break local-controller startup.
- Disposition: applied. The plan now defines `narsor-controller-runtime` as the shared reconciler/identity-client library, keeps the local reconciler in the server lifecycle, and assigns Kubernetes process setup to `narsor-controller`.

### Security: discovery record contains a bearer token

- Evidence: the origin requires restrictive discovery-file permissions (R5); current implementation writes the daemon record with mode `0600`, and the client rejects unprotected files.
- Consequence: weakening file protection could expose the token to other local users.
- Disposition: applied. Requirement R8 and its tests now explicitly preserve atomic publication, owner-only `0600` permissions, ownership checks, and rejection of unprotected records.

### Product: local binary installation layout

- Evidence: client discovery launches a sibling `narsor-server`; the local backend launches `narsor-runner`.
- Consequence: separate binaries must be available together for the default local workflow.
- Disposition: applied. U7 now requires the local client/server/runner set to be co-installed from one workspace version and does not introduce a separate installer/updater.

### Scope: runner as a separate process

- Evidence: the origin describes a local harness child, but the user explicitly requested a separate `narsor-runner` package/process and later confirmed no compatibility alias is needed because Narsor has not been deployed.
- Consequence: deferring the runner split would undo an explicit user decision.
- Disposition: dismissed. Preserve KD4 as session-settled; no change made to runner scope.

## Applied edits

- Added the shared controller-runtime boundary and clarified the distinct local-server and remote-Kubernetes controller lifecycles.
- Made discovery-token file protections explicit and attached checks to the discovery tests.
- Specified sibling resolution for the runner, the local co-installation layout, and the test strategy for binaries belonging to other workspace packages.
- Marked the settled separate-client and runner decisions in the Product Contract.

## Verification

- `git diff --no-index --check /dev/null <plan>` emitted no whitespace errors (exit status 1 is expected because the plan is a new untracked file).
- No build or tests were run; this was a planning/review task.
- Existing user changes in `src/cli/daemon.rs`, `src/daemon/mod.rs`, and `src/daemon/runtime.rs` were left untouched.
