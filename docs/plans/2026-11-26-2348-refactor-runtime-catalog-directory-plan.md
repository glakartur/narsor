---
title: Move the runtime catalogue and its component scripts into one runtime/ directory
type: refactor
status: requirements
date: 2026-11-26
artifact_contract: ce-unified-plan/v1
product_contract_source: ce-brainstorm
execution: code
---

# Move the runtime catalogue and its component scripts into one runtime/ directory

## Goal Capsule

- **Objective:** An operator finds everything that defines the Docker task image (the runtime catalogue and the component scripts it installs) in one directory of the catalogue, and an existing installation keeps working after the update.
- **Means:** move `{catalog}/runtime.yaml` to `{catalog}/runtime/runtime.yaml` and the component scripts from `{catalog}/components/` to `{catalog}/runtime/components/`.
- **Product authority:** the user's request of 2026-11-26 and their answers on the pull request: Q1 option (a); Q2 migration as part of the install script; the install script reads the work mode from the existing configuration.
- **Open blockers:** none.

## Product Contract

### Summary

The catalogue gets a `runtime/` directory that holds `runtime.yaml` and the `components/` scripts it names. Script paths in `runtime.yaml` resolve relative to the directory of `runtime.yaml`, so the existing `components/...` entries stay valid after the move. The install script moves the files of an old layout. Until then, an old layout keeps working with a warning.

### Problem Frame

Today the definition of the Docker task image is split between two places at the catalogue root: `runtime.yaml` and the `components/` directory. The root also holds `backends/`, `profiles/`, `workflows/`, `automations/` and `queues/`, so `components/` reads as one more top-level catalogue kind, which it is not: only `runtime.yaml` uses it. Automations already follow the "one directory per concern" pattern (`automations/{name}/automation.yaml` plus its scripts).

The current script-path rule is also tied to the catalogue root (`crates/narsor-engine/src/environment/profile.rs`, `ScriptSource::text` and `seal`), so moving `runtime.yaml` alone would break every `components/...` entry.

### Key Decisions

- **Script paths resolve relative to the directory of `runtime.yaml`, not the catalogue root.** With the file at the catalogue root this is the same rule as today, so one rule serves both layouts. Governs R3, R4.
- **The old layout stays readable for a transition period. The install script migrates it; the server and `narsor-cli init` do not.** The server does not move operator files; a mounted catalogue can be read-only. Governs R5, R6, R7, R13.
- **Sealed task images do not change.** A sealed `TaskImage` holds script text, not paths (`crates/narsor-model/src/domain/environment.rs`, `TaskImageComponent`), so the move changes no `environment_identity` when the script text stays the same. Governs R9.

### Requirements

**Layout**

- R1. The runtime catalogue is read from `{catalog}/runtime/runtime.yaml` (`.yml` accepted as for other catalogue entries).
- R2. The example catalogue in the repository (`examples/catalog/`) uses the new layout: `examples/catalog/runtime/runtime.yaml` and `examples/catalog/runtime/components/`.

**Script paths**

- R3. A bare-string `install` or `version` script path in `runtime.yaml` resolves relative to the directory that holds the `runtime.yaml` in use.
- R4. A script path must stay inside that directory after symlinks are resolved; a path outside it is refused with the same kind of error as today ("lies outside the catalogue directory", reworded to name the runtime directory).

**Old layout (transition)**

- R5. When `{catalog}/runtime/runtime.yaml` does not exist and `{catalog}/runtime.yaml` does, the old file is used, its script paths resolve per R3 (that is, relative to the catalogue root), and the server logs one warning that names both paths and the move to make.
- R6. When both files exist, resolving a task environment fails with an error that names both paths; neither file wins silently.
- R7. A write of the runtime catalogue through the API (`EnvironmentCatalog` write path in `crates/narsor-engine/src/environment/catalog.rs`) writes to the file in use; with no file present, it writes the new location.

**Tools that create or ship the catalogue**

- R8. `narsor-cli init` (`src/cli/init.rs`) creates `runtime/runtime.yaml` and `runtime/components/...` for a new catalogue. It still never changes an existing runtime catalogue, in either location.
- R9. Existing tasks keep their sealed task image and environment identity; an environment refresh after the move gives the same identity when the script text did not change.
- R10. The ConfigMap import of the catalogue (`docs/operations/remote-backends.md`, "The catalogue in a ConfigMap") places the `runtime.yaml` key at `runtime/runtime.yaml`.

**Install script**

- R13. `dist/install.sh` moves an old layout during install or update: `{catalog}/runtime.yaml` to `{catalog}/runtime/runtime.yaml` and `{catalog}/components/` to `{catalog}/runtime/components/`. It does not overwrite files. When both layouts exist, it stops with an error that names both paths and changes nothing.
- R14. `dist/install.sh` reads the work mode of local tasks (`process` or `docker`) from the existing configuration and offers it as the default answer of the work-mode question. `--local-mode` still overrides it. With no existing configuration, the default stays as today.

**Documentation**

- R11. Operator, user and developer documents that name `{catalog}/runtime.yaml` or `components/` describe the new layout and the transition rule (at least `docs/user/local-docker.md`, `docs/operations/installation.md`, `docs/operations/remote-backends.md`, `docs/operations/updates.md`, `docs/dev/workflow-schema.md`, and the module docs in `crates/narsor-engine/src/environment/{catalog,profile,mod}.rs` and `crates/narsor-store/src/paths.rs`).
- R12. User-facing messages that say "runtime.yaml" stay understandable; a message that tells the operator to edit the file names its path in the new layout.

### Acceptance Examples

- AE1. **Covers R1, R3.** Given `{catalog}/runtime/runtime.yaml` with `install: components/glab/install.sh` and the script at `{catalog}/runtime/components/glab/install.sh`, a Docker-mode task resolves and seals the script text.
- AE2. **Covers R5, R3.** Given only `{catalog}/runtime.yaml` with `install: components/glab/install.sh` and the script at `{catalog}/components/glab/install.sh`, the task resolves as before and the log has one warning about the old location.
- AE3. **Covers R6.** Given both `{catalog}/runtime.yaml` and `{catalog}/runtime/runtime.yaml`, task creation and environment refresh fail with an error that names both files.
- AE4. **Covers R4.** Given the new layout and `install: ../components/x.sh`, sealing is refused because the path leaves the runtime directory.
- AE5. **Covers R9.** Given a task created on the old layout, after the operator moves both parts to `runtime/` without changing script text, `narsor-cli task refresh-env` reports no change.

### Scope Boundaries

- No automatic move of the operator's files by the server or by `narsor-cli init`; only the install script moves them (R13).
- No change to the `runtime.yaml` schema (`bundle`, `tools`, `task_image`) or to how the controller builds images.
- No change to other catalogue kinds (`backends/`, `profiles/`, `workflows/`, `automations/`, `queues/`).
- Deferred: shipping component scripts through the Kubernetes ConfigMap (today a key cannot name a file below the catalogue root other than the listed kinds); a CLI command that moves an old layout; removing the old-layout fallback in a later release.

### Dependencies / Assumptions

- The catalogue path is configurable (`server.catalog`); "`/catalog/`" in the request means that root, and `examples/catalog/` in the repository.
- A future removal of the fallback (R5) needs a release note in `docs/operations/updates.md`.

### Założenia do potwierdzenia

- A1. "`/catalog/`" in the request means the catalogue root (`server.catalog`) and its copy in the repository, `examples/catalog/`; the target names are `runtime/` (singular, as in the proposed tree), `runtime/runtime.yaml` and `runtime/components/`.
- A2. Script paths in `runtime.yaml` become relative to the directory of `runtime.yaml` (Q1, answered), so the entries `components/...` do not change.
- A3. A script must stay inside the runtime directory (R4). A script stored elsewhere in the catalogue must move into `runtime/` or become `{ inline: … }`.
- A4. Old installations keep working with a warning until the install script moves them (Q2, answered); both files present is an error, not a silent choice.
- A5. The server does not move files itself; the install script does (the catalogue can be mounted read-only).
- A8. The existing configuration holds the work mode in a form the install script can read; the plan step must find where.
- A6. The ConfigMap key `runtime.yaml` maps to `runtime/runtime.yaml`; shipping component scripts through the ConfigMap stays out of scope.
- A7. Sealed task images and environment identities do not change, because they hold script text, not paths.

### Pytania otwarte dla użytkownika

- Q1. Answered: option (a). Script paths resolve relative to the directory of `runtime.yaml`.
- Q2. Answered: the install script migrates the old layout (R13); until then the old layout works with a warning (R5).

### Sources / Research

- `crates/narsor-store/src/paths.rs` — `RUNTIME_FILE`, `CatalogPaths::runtime_file`.
- `crates/narsor-engine/src/environment/catalog.rs` — read and write of the runtime catalogue.
- `crates/narsor-engine/src/environment/profile.rs` — `seal(catalog_root)`, `ScriptSource::text`, the path checks.
- `crates/narsor-engine/src/launch.rs` — `seal(&self.catalog_root)` call and error messages that name `runtime.yaml`.
- `dist/install.sh` — install and update flow; work-mode question (`--local-mode`).
- `src/cli/init.rs` — embedded example runtime and component scripts.
- `docs/dev/architecture.md` D81 (KTD8: "a bare string script is a path inside the catalogue"), D83 (environment refresh).
