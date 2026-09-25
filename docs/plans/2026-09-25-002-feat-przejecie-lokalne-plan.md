---
title: "feat: Przejęcie lokalne — samowystarczalne wykonanie, checkpoint i materializacja (M1)"
type: feat
status: active
date: 2026-09-25
origin: docs/brainstorms/2026-09-25-rozproszony-narsor-requirements.md
---

# Przejęcie lokalne — plan wdrożeniowy (M1)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development
> (recommended) or superpowers:executing-plans to implement this plan task-by-task.
> Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wykonanie staje się samowystarczalne i odtwarzalne: niesie przypiętą definicję
i bazowy commit, zapisuje checkpoint workspace'u na granicy kroku, a nowy proces potrafi je
podjąć, odtwarzając workspace **z checkpointu**, nie z ocalałego katalogu.

**Architecture:** Checkpoint to **git bundle delty względem przypiętej bazy**, wytwarzany
w indeksie tymczasowym poza drzewem roboczym, więc indeks użytkownika pozostaje nietknięty.
Ten sam mechanizm obsługuje workspace gitowy (baza = commit, bundle z `--not <base>`)
i katalog bez gita (baza = brak, dedykowane `GIT_DIR` poza drzewem roboczym). Materializacja
to klon na przypiętą bazę, `fetch` bundla i odtworzenie brudnego stanu przez
`reset --hard <wip>` + `reset --mixed <base>`. Reconciliacja przy przejęciu używa istniejącego
`persistence::recovery::reconcile` (D23) — **nie powstaje druga ścieżka odtwarzania**.

**Tech Stack:** Rust 2021, `git` jako proces potomny (D1, README: `git` jest wymaganiem czasu
wykonania), `serde`/`serde_json`, `thiserror`. Bez nowych zależności w `Cargo.toml`.

**Spec:** `docs/brainstorms/2026-09-25-rozproszony-narsor-requirements.md`

## Global Constraints

- **M0 musi być zakończone.** Ten plan zakłada trzy korzenie i `StatePaths`/`WorkPaths` z planu
  `2026-09-25-001-refactor-rozbicie-katalogu-danych-plan.md`.
- **Repozytorium produktowe nie dostaje niczego** (R5): żadnej gałęzi, commita ani tagu nie
  wypycha się do `origin`. Commit checkpointu istnieje wyłącznie lokalnie i w bundlu.
- **Pliki ignorowane przez gita nie są przenoszone** (R24). Jedynym wyjątkiem jest
  `preserve_paths` z definicji workflow.
- **Trzymany jest dokładnie jeden checkpoint** (R25).
- **Materializacja zawsze z checkpointu, nigdy z ocalałego katalogu** (R26).
- **Nic w rekordzie wykonania ani w checkpoincie nie przenosi poświadczeń** (R33).
- **Nazwa refa checkpointu:** `refs/narsor/checkpoint`. Dokładnie ta, w obu ścieżkach.
- **Persystencja zostaje plikowa** (D3). Żadnej bazy danych.
- **Testy nie mają `tempfile`** — crate nie jest zależnością. Katalogi tymczasowe powstają jako
  `std::env::temp_dir().join(format!("narsor-...-{}", uuid::Uuid::new_v4()))`, sprzątane w `Drop`,
  dokładnie jak `TempRoot` w `tests/workspace.rs` i `Harness` w `tests/engine_support/mod.rs`.
- **Istniejące suity muszą przechodzić bez zmiany semantyki**, w szczególności
  `tests/engine_recovery.rs` (Success criterion 4 ze specu). Dlatego checkpointowanie w silniku
  jest **opt-in** (Task 7): domyślnie wyłączone, więc żaden istniejący test nie zaczyna nagle
  uruchamiać `git` na każdym kroku.
- **Komunikaty błędów i komentarze doc po angielsku**, dokumenty planistyczne po polsku —
  konwencja repozytorium.

## Review Focus

Pięć klas wejścia, których spec dotyka, a które łatwo zostawić bez testu. Każda ma przypisane
zadanie i jest w nim pokryta testem.

1. **Plik śledzony w bazie, dodany potem do `.gitignore`.** Musi zostać w checkpoincie —
   `.gitignore` rządzi plikami nowymi, nie śledzonymi. Naiwna implementacja (pusty indeks
   tymczasowy) **gubi taki plik i przy odtworzeniu go kasuje**; zweryfikowane empirycznie.
   → **Task 3**.
2. **Przerwany zapis checkpointu.** Poprzedni checkpoint musi zostać nietknięty i nadal
   nadawać się do materializacji. → **Task 3**.
3. **Bazowy commit nieobecny w zdalnym repozytorium** (force-push, `gc`). Materializacja musi
   zawieść **nazwanym błędem**, a nie wysypem `stderr` z gita. → **Task 4**.
4. **Wykonanie bez zadeklarowanego workspace'u** (pusty katalog, D39 / `FALLBACK_WORKSPACE_NAME`).
   Ten sam mechanizm checkpointu musi je obsłużyć. → **Task 5**.
5. **Zagnieżdżone repozytorium w drzewie roboczym** (submoduł albo przypadkowy `git init`).
   `git add -A` zapisuje je jako gitlink, a materializacja zostawia pusty katalog. Zachowanie
   ma być **udokumentowane i przypięte testem**, a nie odkrywane w produkcji. → **Task 4**.

---

## File Structure

| plik | odpowiedzialność |
|---|---|
| `src/workspace/checkpoint.rs` *(nowy)* | Cała wiedza o wytwarzaniu i odtwarzaniu delty przez `git`. Jedyne miejsce, które zna `refs/narsor/checkpoint`, indeks tymczasowy i `git bundle`. |
| `src/workspace/mod.rs` | Re-eksport nowego modułu. |
| `src/domain/workspace.rs` | `Workspace::base_commit` — przypięta baza delty (R2). |
| `src/persistence/paths.rs` | Ścieżki trzech nowych plików wykonania: definicji, bundla i katalogu roboczego checkpointu. |
| `src/tasks/mod.rs` | Zapis przypiętej definicji i bazowego commita przy starcie; `takeover` jako odpowiednik `start_task`. |
| `src/workflow/schema.rs` | Pole `preserve_paths` w sekcji `globals.workspace`. |
| `src/engine/runner.rs` | Opcjonalny hak checkpointu na granicy kroku. |
| `tests/checkpoint.rs` *(nowy)* | Testy jednostki checkpointu: capture, materialise, round-trip, przypadki z Review Focus. |
| `tests/takeover.rs` *(nowy)* | AE1 — równoważność przerwania, sparametryzowana po punkcie przerwania. |

Checkpoint mieszka w **korzeniu stanu**, obok wykonania (`{state}/executions/{id}/`), bo jest
tym, co w M2 poleci do centrali razem ze stanem. Nie powstaje czwarty korzeń.

---

### Task 1: Przypięta baza delty w rekordzie workspace'u

**Files:**
- Modify: `src/domain/workspace.rs:11-32` (struktura `Workspace`)
- Test: `tests/persistence.rs`

**Interfaces:**
- Consumes: nic.
- Produces: `Workspace::base_commit: Option<String>` — czytane przez Task 3, 4, 7 i 8.

- [ ] **Step 1: Write the failing test**

Dopisz na końcu `tests/persistence.rs`:

```rust
#[test]
fn workspace_without_a_base_commit_still_reads() {
    // A state.json written before this field existed must keep loading:
    // an execution in flight when the binary is upgraded is the normal case.
    let json = serde_json::json!({
        "id": "ws-1",
        "workspace_type": "local_folder",
        "path": "/tmp/ws",
    });

    let workspace: narsor::domain::Workspace =
        serde_json::from_value(json).expect("deserialize legacy workspace");

    assert_eq!(workspace.base_commit, None);
}

#[test]
fn workspace_round_trips_its_base_commit() {
    let mut workspace = narsor::domain::Workspace::local_folder("/tmp/ws".to_string());
    workspace.base_commit = Some("0123456789abcdef0123456789abcdef01234567".to_string());

    let encoded = serde_json::to_value(&workspace).expect("serialize");
    let decoded: narsor::domain::Workspace =
        serde_json::from_value(encoded).expect("deserialize");

    assert_eq!(
        decoded.base_commit.as_deref(),
        Some("0123456789abcdef0123456789abcdef01234567")
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test persistence workspace_ -- --nocapture`
Expected: FAIL — `no field 'base_commit' on type 'Workspace'`.

- [ ] **Step 3: Write minimal implementation**

W `src/domain/workspace.rs`, w strukturze `Workspace`, po polu `git_ref`:

```rust
    /// The commit the workspace was materialised at, recorded once when the
    /// execution started.
    ///
    /// This is the base a checkpoint delta is measured against, and it is
    /// deliberately *not* re-read from the checkout: a checkout that has moved
    /// on would silently change what "the delta" means. `None` for a workspace
    /// that is not a git work tree, and for executions written before this
    /// field existed.
    #[serde(default)]
    pub base_commit: Option<String>,
```

Dodaj `base_commit: None` w trzech konstruktorach: `local_folder`, `git_checkout`,
`git_worktree`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --test persistence workspace_`
Expected: PASS (2 testy).

- [ ] **Step 5: Run the whole suite**

Run: `cargo test`
Expected: PASS — pole ma `#[serde(default)]`, więc nic istniejącego się nie psuje.

- [ ] **Step 6: Commit**

```bash
git add src/domain/workspace.rs tests/persistence.rs
git commit -m "feat(domain): pin the commit a workspace was materialised at"
```

---

### Task 2: Przypięta definicja w katalogu wykonania (R2, AE7)

**Files:**
- Modify: `src/persistence/paths.rs` (stała + metoda na `StatePaths`)
- Modify: `src/tasks/mod.rs:420-600` (zapis przy starcie, odczyt w `resolve_definition`)
- Test: `tests/tasks.rs`

**Interfaces:**
- Consumes: `StatePaths` z M0.
- Produces: `StatePaths::definition_file(&self, execution_id: &str) -> PathBuf`;
  `resolve_definition` czyta przypiętą kopię, zanim sięgnie po nazwę.

- [ ] **Step 1: Write the failing test**

Dopisz do `tests/tasks.rs`:

```rust
#[test]
fn a_started_execution_keeps_its_own_definition() {
    // AE7: the catalogue may later hold a different definition under the same
    // name. The execution must go on running the one it started with (D31).
    let temp = TempRoot::new();
    let catalog = temp.join("catalog");
    std::fs::create_dir_all(catalog.join("workflows")).expect("create workflows dir");

    let definition = catalog.join("workflows").join("greet.yaml");
    std::fs::write(&definition, ORIGINAL_DEFINITION).expect("write definition");

    let store = narsor::persistence::ExecutionStore::new(temp.join("state"));
    let execution = start_greet_task(&store, &definition);

    // The catalogue drifts underneath the running execution.
    std::fs::write(&definition, DRIFTED_DEFINITION).expect("overwrite definition");

    let resolved = narsor::tasks::resolve_definition(&catalog, &execution)
        .expect("resolve the pinned definition");

    assert!(
        resolved.source.contains("original-marker"),
        "expected the pinned definition, got: {}",
        resolved.source
    );
}
```

Gdzie `ORIGINAL_DEFINITION` i `DRIFTED_DEFINITION` to dwie stałe różniące się polem
`description` (`original-marker` / `drifted-marker`), a `start_greet_task` to lokalny helper
wołający `narsor::tasks::start_task` z pustymi inputami. Jeśli helper o tej roli już
istnieje w pliku, użyj go zamiast pisać drugi.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test tasks a_started_execution_keeps_its_own_definition`
Expected: FAIL — `resolved.source` zawiera `drifted-marker`, bo definicja jest wczytywana
ponownie z katalogu.

- [ ] **Step 3: Add the path**

W `src/persistence/paths.rs`, obok `STATE_FILE` i `CONTEXT_FILE`:

```rust
/// The definition an execution was started from, pinned inside its directory.
///
/// An execution is resumable from its own record alone, so the definition
/// travels with it rather than being looked up by name in a catalogue that may
/// have moved on (D31).
pub const DEFINITION_FILE: &str = "definition.yaml";
```

i w `impl StatePaths`, obok `context_file`:

```rust
    /// `{state}/executions/{id}/definition.yaml`.
    pub fn definition_file(&self, execution_id: &str) -> PathBuf {
        self.execution_dir(execution_id).join(DEFINITION_FILE)
    }
```

- [ ] **Step 4: Write the definition when the task starts**

W `src/tasks/mod.rs`, w `start_task`, bezpośrednio po utworzeniu katalogów wykonania
i przed pierwszym `save_execution`, zapisz źródło definicji:

```rust
    // The execution carries its own definition from here on (R2): resuming it
    // must not depend on a catalogue that may have changed, or on a worker that
    // may not have the catalogue at all.
    write_atomic(
        &store.paths().definition_file(&execution.id),
        source.text().as_bytes(),
    )?;
```

Użyj tej nazwy, pod którą źródło YAML jest w tej funkcji dostępne; jeśli dostępna jest tylko
ścieżka pliku, wczytaj go przez `std::fs::read_to_string` w tym samym miejscu.
`write_atomic` pochodzi z `crate::persistence`.

- [ ] **Step 5: Prefer the pinned copy when resolving**

W `src/tasks/mod.rs`, w `resolve_definition`, **przed** dotychczasowym blokiem
`stored_definition_path`, dodaj:

```rust
    // The pinned copy wins over everything else: it is the definition this
    // execution actually started with.
    let pinned = store_paths.definition_file(&execution.id);
    if pinned.is_file() {
        let source = fs::read_to_string(&pinned).map_err(|source| DefinitionLookupError::Read {
            path: pinned.clone(),
            source,
        })?;
        return Ok(ResolvedWorkflow {
            name: stored_definition_name(execution).unwrap_or_else(|| execution.workflow_id.clone()),
            origin: WorkflowOrigin::File(pinned),
            source,
        });
    }
```

`resolve_definition` przyjmuje dziś `catalog: &Path`; rozszerz sygnaturę o `store_paths: &StatePaths`
i popraw wszystkich wołających (`src/cli/commands.rs`, `src/tui/app.rs:1334`). Stary blok
zostaje **nietknięty** — jest ścieżką zgodności dla wykonań sprzed tej zmiany.

- [ ] **Step 6: Run the test to verify it passes**

Run: `cargo test --test tasks a_started_execution_keeps_its_own_definition`
Expected: PASS.

- [ ] **Step 7: Run the whole suite**

Run: `cargo test`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add src/persistence/paths.rs src/tasks/mod.rs src/cli/commands.rs src/tui/app.rs tests/tasks.rs
git commit -m "feat(tasks): pin the definition an execution started with"
```

---

### Task 3: Wytworzenie checkpointu (R22, R23, R24, R25)

**Files:**
- Create: `src/workspace/checkpoint.rs`
- Modify: `src/workspace/mod.rs:20-25` (re-eksport)
- Modify: `src/persistence/paths.rs` (dwie stałe + dwie metody)
- Test: `tests/checkpoint.rs` *(nowy)*

**Interfaces:**
- Consumes: `Workspace::base_commit` (Task 1).
- Produces:
  - `pub const CHECKPOINT_REF: &str = "refs/narsor/checkpoint";`
  - `pub enum Base { Commit(String), None }`
  - `pub struct CaptureRequest<'a> { pub work_tree: &'a Path, pub base: Base, pub scratch: &'a Path, pub bundle: &'a Path, pub preserve: &'a [String] }`
  - `pub fn capture(request: CaptureRequest<'_>) -> Result<Capture, CheckpointError>`
  - `pub struct Capture { pub commit: String, pub bytes: u64 }`
  - `pub enum CheckpointError { Git { command: String, stderr: String }, MissingBase { commit: String }, Io(std::io::Error) }`
  - `StatePaths::checkpoint_file(&self, execution_id: &str) -> PathBuf`
  - `StatePaths::checkpoint_scratch(&self, execution_id: &str) -> PathBuf`

- [ ] **Step 1: Write the failing tests**

Utwórz `tests/checkpoint.rs`:

```rust
//! The checkpoint: a git bundle of the delta between a pinned base and the
//! work tree as it stands.
//!
//! Everything runs against a real filesystem and a real `git`, in a temporary
//! directory — never inside the repository.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use narsor::workspace::{capture, Base, CaptureRequest};

/// A temporary directory removed when it goes away.
struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("narsor-checkpoint-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).expect("create temp root");
        Self { path }
    }

    fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=Checkpoint Test"])
        .args(["-c", "user.email=checkpoint@example.com"])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap_or_else(|error| panic!("run git {args:?}: {error}"));
    assert!(
        output.status.success(),
        "git {args:?} failed in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// A repository with one commit, and the sha of that commit.
fn repo_with_base(dir: &Path) -> String {
    fs::create_dir_all(dir).expect("create repo");
    git(dir, &["-c", "init.defaultBranch=main", "init"]);
    fs::write(dir.join("tracked.txt"), "base\n").expect("write tracked");
    fs::write(dir.join(".gitignore"), "ignored.log\n").expect("write gitignore");
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "--message", "base"]);
    git(dir, &["rev-parse", "HEAD"])
}

#[test]
fn capture_leaves_the_working_index_untouched() {
    let temp = TempRoot::new();
    let work = temp.join("work");
    let base = repo_with_base(&work);

    fs::write(work.join("tracked.txt"), "base\nmodified\n").expect("modify");
    fs::write(work.join("untracked.txt"), "new\n").expect("add untracked");

    let before = git(&work, &["status", "--porcelain"]);

    capture(CaptureRequest {
        work_tree: &work,
        base: Base::Commit(base),
        scratch: &temp.join("scratch"),
        bundle: &temp.join("cp.bundle"),
        preserve: &[],
    })
    .expect("capture");

    assert_eq!(
        git(&work, &["status", "--porcelain"]),
        before,
        "capture must not disturb the user's index or work tree"
    );
}

#[test]
fn capture_excludes_ignored_files() {
    let temp = TempRoot::new();
    let work = temp.join("work");
    let base = repo_with_base(&work);

    fs::write(work.join("ignored.log"), "noise\n").expect("write ignored");

    let capture_result = capture(CaptureRequest {
        work_tree: &work,
        base: Base::Commit(base),
        scratch: &temp.join("scratch"),
        bundle: &temp.join("cp.bundle"),
        preserve: &[],
    })
    .expect("capture");

    let listing = git(
        &work,
        &["ls-tree", "-r", "--name-only", &capture_result.commit],
    );
    assert!(
        !listing.contains("ignored.log"),
        "an ignored file must not travel in the checkpoint, got: {listing}"
    );
}

#[test]
fn capture_keeps_a_tracked_file_that_became_ignored() {
    // Review Focus 1. `.gitignore` governs files that are new, not files that
    // are already tracked. Seeding the temporary index from the base is what
    // makes this true; an empty index silently drops the file, and restoring
    // then deletes it.
    let temp = TempRoot::new();
    let work = temp.join("work");
    let base = repo_with_base(&work);

    fs::write(work.join("kept.txt"), "tracked\n").expect("write kept");
    git(&work, &["add", "--", "kept.txt"]);
    git(&work, &["commit", "--message", "track kept.txt"]);
    fs::write(work.join(".gitignore"), "ignored.log\nkept.txt\n").expect("ignore kept.txt");
    fs::write(work.join("kept.txt"), "tracked and edited\n").expect("edit kept");

    let head = git(&work, &["rev-parse", "HEAD"]);
    let capture_result = capture(CaptureRequest {
        work_tree: &work,
        base: Base::Commit(head),
        scratch: &temp.join("scratch"),
        bundle: &temp.join("cp.bundle"),
        preserve: &[],
    })
    .expect("capture");

    let listing = git(
        &work,
        &["ls-tree", "-r", "--name-only", &capture_result.commit],
    );
    assert!(
        listing.contains("kept.txt"),
        "a tracked file added to .gitignore must stay in the checkpoint, got: {listing}"
    );
}

#[test]
fn a_failed_capture_leaves_the_previous_bundle_intact() {
    // Review Focus 2.
    let temp = TempRoot::new();
    let work = temp.join("work");
    let base = repo_with_base(&work);
    let bundle = temp.join("cp.bundle");

    capture(CaptureRequest {
        work_tree: &work,
        base: Base::Commit(base),
        scratch: &temp.join("scratch"),
        bundle: &bundle,
        preserve: &[],
    })
    .expect("first capture");

    let good = fs::read(&bundle).expect("read first bundle");

    // A base that does not exist makes git fail part-way.
    let failed = capture(CaptureRequest {
        work_tree: &work,
        base: Base::Commit("0".repeat(40)),
        scratch: &temp.join("scratch"),
        bundle: &bundle,
        preserve: &[],
    });

    assert!(failed.is_err(), "capture against a missing base must fail");
    assert_eq!(
        fs::read(&bundle).expect("read bundle after the failure"),
        good,
        "a failed capture must leave the previous checkpoint byte-identical"
    );
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test checkpoint`
Expected: FAIL — `unresolved import 'narsor::workspace::capture'`.

- [ ] **Step 3: Add the paths**

W `src/persistence/paths.rs`:

```rust
/// The one checkpoint kept for an execution (R25).
pub const CHECKPOINT_FILE: &str = "checkpoint.bundle";

/// Git's own scratch area for producing and applying a checkpoint.
///
/// Deliberately outside the work tree, so that a workspace which is not a git
/// repository does not acquire a `.git` directory it never asked for.
pub const CHECKPOINT_SCRATCH_DIR: &str = "checkpoint.git";
```

i w `impl StatePaths`:

```rust
    /// `{state}/executions/{id}/checkpoint.bundle`.
    pub fn checkpoint_file(&self, execution_id: &str) -> PathBuf {
        self.execution_dir(execution_id).join(CHECKPOINT_FILE)
    }

    /// `{state}/executions/{id}/checkpoint.git`.
    pub fn checkpoint_scratch(&self, execution_id: &str) -> PathBuf {
        self.execution_dir(execution_id).join(CHECKPOINT_SCRATCH_DIR)
    }
```

- [ ] **Step 4: Write the module**

Utwórz `src/workspace/checkpoint.rs`:

```rust
//! The checkpoint: the difference between a pinned base and the work tree.
//!
//! # Why a git bundle and not a diff
//!
//! A checkpoint has to carry binary files, mode changes, deletions and renames
//! — everything a work tree can hold. That is precisely the set git already
//! serialises, so the checkpoint is a `git bundle` of one commit, thin against
//! the base (`--not <base>`), and what travels is the delta and nothing else.
//!
//! # Why the index is temporary
//!
//! Producing the commit stages the whole work tree, which would destroy the
//! staging area the person is working in. `GIT_INDEX_FILE` points the operation
//! at an index outside the work tree, so the user's index is never touched.
//!
//! # Why the temporary index is seeded from the base
//!
//! An *empty* temporary index makes `.gitignore` authoritative over
//! tracked-ness: a file that is tracked in the base and later added to
//! `.gitignore` is simply not staged, vanishes from the checkpoint, and is
//! **deleted** when the checkpoint is restored. Seeding with `read-tree <base>`
//! keeps tracked files tracked, and leaves the ignore rules governing only what
//! is genuinely new — which is what they are for.
//!
//! # The same mechanism for a directory that is not a repository
//!
//! [`Base::None`] points `GIT_DIR` at a scratch repository beside the execution
//! and `GIT_WORK_TREE` at the directory, so a workflow with no repository at
//! all (`DOMAIN_MODEL.md` §2.8, D39) checkpoints through this very code with no
//! second path — and without a `.git` appearing in its workspace.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use thiserror::Error;

/// The ref a checkpoint commit is published under, inside the bundle.
pub const CHECKPOINT_REF: &str = "refs/narsor/checkpoint";

/// What the delta is measured against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Base {
    /// A git work tree whose object store already holds this commit.
    Commit(String),
    /// A directory that is not a git work tree; the whole content is the delta.
    None,
}

/// Everything [`capture`] needs.
#[derive(Debug)]
pub struct CaptureRequest<'a> {
    /// The directory whose contents are captured.
    pub work_tree: &'a Path,
    /// The base the delta is measured from.
    pub base: Base,
    /// Git's scratch area: the temporary index, and the repository for
    /// [`Base::None`]. Created if missing.
    pub scratch: &'a Path,
    /// Where the bundle is written. Replaced atomically.
    pub bundle: &'a Path,
    /// Paths force-added despite `.gitignore` (R24's only exception).
    pub preserve: &'a [String],
}

/// What one capture produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capture {
    /// The checkpoint commit.
    pub commit: String,
    /// Size of the bundle on disk.
    pub bytes: u64,
}

/// Everything that can stop a checkpoint from being produced or applied.
#[derive(Debug, Error)]
pub enum CheckpointError {
    /// A `git` invocation failed.
    #[error("git {command} failed: {stderr}")]
    Git {
        /// The subcommand, for the message.
        command: String,
        /// What git said.
        stderr: String,
    },

    /// The base commit is not in the repository the checkpoint is applied to.
    ///
    /// The usual cause is a force-push or a `gc` on the remote between the
    /// checkpoint being taken and being restored.
    #[error("the base commit {commit} is not present; the remote may have been force-pushed")]
    MissingBase {
        /// The commit that could not be found.
        commit: String,
    },

    /// A filesystem operation failed.
    #[error("checkpoint I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

/// Runs one `git` invocation with the checkpoint's environment.
fn run(
    work_tree: &Path,
    git_dir: Option<&Path>,
    index: Option<&Path>,
    args: &[&str],
) -> Result<String, CheckpointError> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(work_tree)
        .args(["-c", "user.name=narsor"])
        .args(["-c", "user.email=narsor@localhost"])
        .args(["-c", "commit.gpgsign=false"])
        .args(["-c", "core.hooksPath=/dev/null"])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0");

    if let Some(dir) = git_dir {
        command.env("GIT_DIR", dir).env("GIT_WORK_TREE", work_tree);
    }
    if let Some(index) = index {
        command.env("GIT_INDEX_FILE", index);
    }

    let output = command.output()?;
    if !output.status.success() {
        return Err(CheckpointError::Git {
            command: args.join(" "),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Writes the checkpoint of `work_tree` to `bundle`, replacing any previous one.
///
/// The bundle is written to a unique temporary name in the same directory and
/// renamed into place, so an interrupted or failing capture leaves the previous
/// checkpoint byte-identical (R25).
pub fn capture(request: CaptureRequest<'_>) -> Result<Capture, CheckpointError> {
    fs::create_dir_all(request.scratch)?;
    let index = request.scratch.join("index");
    let _ = fs::remove_file(&index);

    let git_dir = match &request.base {
        Base::Commit(_) => None,
        Base::None => {
            let dir = request.scratch.join("repo.git");
            if !dir.exists() {
                fs::create_dir_all(&dir)?;
                run(request.work_tree, Some(&dir), None, &["init", "--quiet"])?;
            }
            Some(dir)
        }
    };
    let git_dir = git_dir.as_deref();

    if let Base::Commit(commit) = &request.base {
        run(
            request.work_tree,
            git_dir,
            Some(&index),
            &["read-tree", commit],
        )
        .map_err(|_| CheckpointError::MissingBase {
            commit: commit.clone(),
        })?;
    }

    run(
        request.work_tree,
        git_dir,
        Some(&index),
        &["add", "--all", "--", "."],
    )?;
    for path in request.preserve {
        run(
            request.work_tree,
            git_dir,
            Some(&index),
            &["add", "--force", "--", path.as_str()],
        )?;
    }

    let tree = run(request.work_tree, git_dir, Some(&index), &["write-tree"])?;

    let mut commit_args = vec!["commit-tree".to_string(), tree];
    if let Base::Commit(base) = &request.base {
        commit_args.push("-p".to_string());
        commit_args.push(base.clone());
    }
    commit_args.push("-m".to_string());
    commit_args.push("narsor checkpoint".to_string());
    let commit_argv: Vec<&str> = commit_args.iter().map(String::as_str).collect();
    let commit = run(request.work_tree, git_dir, Some(&index), &commit_argv)?;

    run(
        request.work_tree,
        git_dir,
        Some(&index),
        &["update-ref", CHECKPOINT_REF, &commit],
    )?;

    let parent = request
        .bundle
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    fs::create_dir_all(&parent)?;
    let staging = parent.join(format!("checkpoint-{}.bundle", uuid::Uuid::new_v4()));

    let mut bundle_args = vec![
        "bundle".to_string(),
        "create".to_string(),
        "--quiet".to_string(),
        staging.display().to_string(),
        CHECKPOINT_REF.to_string(),
    ];
    if let Base::Commit(base) = &request.base {
        bundle_args.push("--not".to_string());
        bundle_args.push(base.clone());
    }
    let bundle_argv: Vec<&str> = bundle_args.iter().map(String::as_str).collect();

    if let Err(error) = run(request.work_tree, git_dir, Some(&index), &bundle_argv) {
        let _ = fs::remove_file(&staging);
        return Err(error);
    }

    let bytes = fs::metadata(&staging)?.len();
    fs::rename(&staging, request.bundle)?;

    Ok(Capture { commit, bytes })
}
```

W `src/workspace/mod.rs` dopisz moduł i re-eksport:

```rust
pub mod checkpoint;

pub use checkpoint::{capture, Base, Capture, CaptureRequest, CheckpointError, CHECKPOINT_REF};
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --test checkpoint`
Expected: PASS (4 testy).

- [ ] **Step 6: Commit**

```bash
git add src/workspace/checkpoint.rs src/workspace/mod.rs src/persistence/paths.rs tests/checkpoint.rs
git commit -m "feat(workspace): capture a work tree as a bundle of its delta"
```

---

### Task 4: Materializacja z checkpointu (R26)

**Files:**
- Modify: `src/workspace/checkpoint.rs` (dopisz `materialise`)
- Modify: `src/workspace/mod.rs` (re-eksport)
- Test: `tests/checkpoint.rs`

**Interfaces:**
- Consumes: `capture`, `Base`, `CheckpointError` (Task 3).
- Produces:
  - `pub struct MaterialiseRequest<'a> { pub work_tree: &'a Path, pub base: Base, pub scratch: &'a Path, pub bundle: &'a Path }`
  - `pub fn materialise(request: MaterialiseRequest<'_>) -> Result<(), CheckpointError>`

- [ ] **Step 1: Write the failing tests**

Dopisz do `tests/checkpoint.rs`:

```rust
use narsor::workspace::{materialise, MaterialiseRequest};

/// Clones `origin` into `into` and checks `base` out, the way a takeover does.
fn clone_at(origin: &Path, into: &Path, base: &str) {
    let parent = into.parent().expect("clone target has a parent");
    fs::create_dir_all(parent).expect("create clone parent");
    let output = Command::new("git")
        .arg("clone")
        .arg("--quiet")
        .arg(origin)
        .arg(into)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("run git clone");
    assert!(output.status.success(), "git clone failed");
    git(into, &["checkout", "--quiet", base]);
}

#[test]
fn a_dirty_tree_survives_a_round_trip() {
    // Success criterion 3: the restored tree is identical to the captured one,
    // and `git status` reports the same dirty state, not a pile of staged work.
    let temp = TempRoot::new();
    let origin = temp.join("origin");
    let base = repo_with_base(&origin);

    let work = temp.join("work");
    clone_at(&origin, &work, &base);

    fs::write(work.join("tracked.txt"), "base\nmodified\n").expect("modify tracked");
    fs::write(work.join("untracked.txt"), "brand new\n").expect("add untracked");
    fs::write(work.join("binary.dat"), [0u8, 1, 2, 255]).expect("add binary");
    fs::remove_file(work.join(".gitignore")).expect("delete a tracked file");
    let expected_status = git(&work, &["status", "--porcelain"]);

    let bundle = temp.join("cp.bundle");
    capture(CaptureRequest {
        work_tree: &work,
        base: Base::Commit(base.clone()),
        scratch: &temp.join("scratch"),
        bundle: &bundle,
        preserve: &[],
    })
    .expect("capture");

    let fresh = temp.join("fresh");
    clone_at(&origin, &fresh, &base);
    materialise(MaterialiseRequest {
        work_tree: &fresh,
        base: Base::Commit(base),
        scratch: &temp.join("fresh-scratch"),
        bundle: &bundle,
    })
    .expect("materialise");

    assert_eq!(
        git(&fresh, &["status", "--porcelain"]),
        expected_status,
        "the restored tree must show the same dirty state"
    );
    assert_eq!(
        fs::read_to_string(fresh.join("tracked.txt")).expect("read tracked"),
        "base\nmodified\n"
    );
    assert_eq!(
        fs::read_to_string(fresh.join("untracked.txt")).expect("read untracked"),
        "brand new\n"
    );
    assert_eq!(
        fs::read(fresh.join("binary.dat")).expect("read binary"),
        vec![0u8, 1, 2, 255]
    );
    assert!(!fresh.join(".gitignore").exists(), "the deletion must travel");
}

#[test]
fn materialising_without_the_base_is_a_named_error() {
    // Review Focus 3: a force-pushed or garbage-collected base must produce a
    // sentence an operator can act on, not a wall of git stderr.
    let temp = TempRoot::new();
    let origin = temp.join("origin");
    let base = repo_with_base(&origin);

    let work = temp.join("work");
    clone_at(&origin, &work, &base);
    fs::write(work.join("tracked.txt"), "base\nmodified\n").expect("modify");

    let bundle = temp.join("cp.bundle");
    capture(CaptureRequest {
        work_tree: &work,
        base: Base::Commit(base.clone()),
        scratch: &temp.join("scratch"),
        bundle: &bundle,
        preserve: &[],
    })
    .expect("capture");

    // A fresh repository that has never heard of the base.
    let stranger = temp.join("stranger");
    fs::create_dir_all(&stranger).expect("create stranger");
    git(&stranger, &["-c", "init.defaultBranch=main", "init"]);

    let error = materialise(MaterialiseRequest {
        work_tree: &stranger,
        base: Base::Commit(base.clone()),
        scratch: &temp.join("stranger-scratch"),
        bundle: &bundle,
    })
    .expect_err("materialising without the base must fail");

    let message = error.to_string();
    assert!(
        message.contains(&base) && message.contains("force-pushed"),
        "expected a named MissingBase error, got: {message}"
    );
}

#[test]
fn a_nested_repository_restores_as_an_empty_directory() {
    // Review Focus 5. `git add -A` records a nested repository as a gitlink, so
    // the restore leaves the directory empty. Pinned here so the behaviour is
    // known rather than discovered in production.
    let temp = TempRoot::new();
    let origin = temp.join("origin");
    let base = repo_with_base(&origin);

    let work = temp.join("work");
    clone_at(&origin, &work, &base);

    let nested = work.join("vendor");
    fs::create_dir_all(&nested).expect("create nested");
    git(&nested, &["-c", "init.defaultBranch=main", "init"]);
    fs::write(nested.join("inner.txt"), "inner\n").expect("write inner");
    git(&nested, &["add", "-A"]);
    git(&nested, &["commit", "--message", "inner"]);

    let bundle = temp.join("cp.bundle");
    capture(CaptureRequest {
        work_tree: &work,
        base: Base::Commit(base.clone()),
        scratch: &temp.join("scratch"),
        bundle: &bundle,
        preserve: &[],
    })
    .expect("capture");

    let fresh = temp.join("fresh");
    clone_at(&origin, &fresh, &base);
    materialise(MaterialiseRequest {
        work_tree: &fresh,
        base: Base::Commit(base),
        scratch: &temp.join("fresh-scratch"),
        bundle: &bundle,
    })
    .expect("materialise");

    assert!(
        !fresh.join("vendor").join("inner.txt").exists(),
        "a nested repository's contents do not travel in the checkpoint"
    );
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test checkpoint`
Expected: FAIL — `unresolved import 'narsor::workspace::materialise'`.

- [ ] **Step 3: Write the implementation**

Dopisz na końcu `src/workspace/checkpoint.rs`:

```rust
/// Everything [`materialise`] needs.
#[derive(Debug)]
pub struct MaterialiseRequest<'a> {
    /// The directory the checkpoint is restored into. For [`Base::Commit`] it
    /// must already be a checkout holding that commit.
    pub work_tree: &'a Path,
    /// The base the delta was measured from.
    pub base: Base,
    /// Git's scratch area, as in [`CaptureRequest`].
    pub scratch: &'a Path,
    /// The bundle to restore.
    pub bundle: &'a Path,
}

/// Restores a checkpoint into `work_tree`.
///
/// The work tree ends up holding exactly what it held when the checkpoint was
/// taken, and `git status` reports the same dirty state: the checkpoint commit
/// is checked out hard, then the index is reset back to the base, which turns
/// its content back into unstaged modifications and untracked files.
pub fn materialise(request: MaterialiseRequest<'_>) -> Result<(), CheckpointError> {
    fs::create_dir_all(request.scratch)?;

    let git_dir = match &request.base {
        Base::Commit(_) => None,
        Base::None => {
            let dir = request.scratch.join("repo.git");
            if !dir.exists() {
                fs::create_dir_all(&dir)?;
                run(request.work_tree, Some(&dir), None, &["init", "--quiet"])?;
            }
            Some(dir)
        }
    };
    let git_dir = git_dir.as_deref();

    if let Base::Commit(commit) = &request.base {
        run(
            request.work_tree,
            git_dir,
            None,
            &["cat-file", "-e", &format!("{commit}^{{commit}}")],
        )
        .map_err(|_| CheckpointError::MissingBase {
            commit: commit.clone(),
        })?;
    }

    let refspec = format!("{CHECKPOINT_REF}:{CHECKPOINT_REF}");
    run(
        request.work_tree,
        git_dir,
        None,
        &[
            "fetch",
            "--quiet",
            &request.bundle.display().to_string(),
            &refspec,
        ],
    )?;

    run(
        request.work_tree,
        git_dir,
        None,
        &["reset", "--quiet", "--hard", CHECKPOINT_REF],
    )?;

    if let Base::Commit(base) = &request.base {
        run(
            request.work_tree,
            git_dir,
            None,
            &["reset", "--quiet", "--mixed", base],
        )?;
    }

    Ok(())
}
```

Rozszerz re-eksport w `src/workspace/mod.rs` o `materialise` i `MaterialiseRequest`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --test checkpoint`
Expected: PASS (7 testów).

- [ ] **Step 5: Commit**

```bash
git add src/workspace/checkpoint.rs src/workspace/mod.rs tests/checkpoint.rs
git commit -m "feat(workspace): restore a work tree from its checkpoint"
```

---

### Task 5: Workspace bez repozytorium (R23, AE8)

**Files:**
- Test: `tests/checkpoint.rs`
- Modify: `src/workspace/checkpoint.rs` tylko jeśli test to wymusi

**Interfaces:**
- Consumes: `Base::None`, `capture`, `materialise` (Task 3, 4).
- Produces: nic nowego — zadanie dowodzi, że ta sama para funkcji obsługuje przypadek bez gita.

- [ ] **Step 1: Write the failing test**

Dopisz do `tests/checkpoint.rs`:

```rust
#[test]
fn a_plain_directory_round_trips_without_becoming_a_repository() {
    // AE8 / Review Focus 4: a workflow that declares no workspace gets an empty
    // directory (DOMAIN_MODEL.md §2.8). It must checkpoint through the very same
    // code — and must not grow a `.git` of its own.
    let temp = TempRoot::new();
    let work = temp.join("plain");
    fs::create_dir_all(work.join("sub")).expect("create plain workspace");
    fs::write(work.join("a.txt"), "hello\n").expect("write a");
    fs::write(work.join("sub").join("b.dat"), [0u8, 9, 9]).expect("write b");

    let bundle = temp.join("cp.bundle");
    capture(CaptureRequest {
        work_tree: &work,
        base: Base::None,
        scratch: &temp.join("scratch"),
        bundle: &bundle,
        preserve: &[],
    })
    .expect("capture a plain directory");

    assert!(
        !work.join(".git").exists(),
        "checkpointing must not turn the workspace into a repository"
    );

    let fresh = temp.join("fresh-plain");
    fs::create_dir_all(&fresh).expect("create fresh");
    materialise(MaterialiseRequest {
        work_tree: &fresh,
        base: Base::None,
        scratch: &temp.join("fresh-scratch"),
        bundle: &bundle,
    })
    .expect("materialise a plain directory");

    assert_eq!(
        fs::read_to_string(fresh.join("a.txt")).expect("read a"),
        "hello\n"
    );
    assert_eq!(
        fs::read(fresh.join("sub").join("b.dat")).expect("read b"),
        vec![0u8, 9, 9]
    );
    assert!(!fresh.join(".git").exists(), "no .git in the restored tree");
}
```

- [ ] **Step 2: Run test**

Run: `cargo test --test checkpoint a_plain_directory_round_trips`
Expected: PASS bez zmian w kodzie — `Base::None` jest już obsłużone. Jeśli **nie** przechodzi,
napraw `checkpoint.rs`, nie test: to jest dowód na to, że mechanizm jest jeden.

- [ ] **Step 3: Commit**

```bash
git add tests/checkpoint.rs
git commit -m "test(workspace): pin that a workspace without git checkpoints the same way"
```

---

### Task 6: `preserve_paths` — jedyny wyjątek od R24

**Files:**
- Modify: `src/workflow/schema.rs` (pole w sekcji workspace'u)
- Modify: `src/workflow/loader.rs` (przeniesienie do modelu domenowego)
- Test: `tests/checkpoint.rs`, `tests/schema_loading.rs`

**Interfaces:**
- Consumes: `CaptureRequest::preserve` (Task 3).
- Produces: `globals.workspace.preserve_paths: Vec<String>` w schemacie i w modelu.

- [ ] **Step 1: Write the failing test**

Dopisz do `tests/checkpoint.rs`:

```rust
#[test]
fn preserve_paths_carries_an_ignored_file() {
    // R24's only exception: a generated file that the repository ignores but
    // the workflow needs — the classic case being a `.env` written by a step.
    let temp = TempRoot::new();
    let work = temp.join("work");
    let base = repo_with_base(&work);

    fs::write(work.join("ignored.log"), "needed\n").expect("write ignored");

    let capture_result = capture(CaptureRequest {
        work_tree: &work,
        base: Base::Commit(base),
        scratch: &temp.join("scratch"),
        bundle: &temp.join("cp.bundle"),
        preserve: &["ignored.log".to_string()],
    })
    .expect("capture");

    let listing = git(
        &work,
        &["ls-tree", "-r", "--name-only", &capture_result.commit],
    );
    assert!(
        listing.contains("ignored.log"),
        "a preserved path must travel despite .gitignore, got: {listing}"
    );
}
```

Oraz test schematu w `tests/schema_loading.rs`, w stylu tego pliku: definicja z
`globals: { workspace: { type: local, path: ".", preserve_paths: [".env"] } }` kompiluje się,
a `preserve_paths` dociera do modelu domenowego; definicja bez tego pola daje pustą listę.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test checkpoint preserve_paths_carries_an_ignored_file && cargo test --test schema_loading preserve`
Expected: pierwszy PASS (`preserve` jest już parametrem `capture`), drugi FAIL — schemat nie zna
pola.

- [ ] **Step 3: Add the field to the schema**

W `src/workflow/schema.rs`, w strukturze opisującej `globals.workspace`:

```rust
    /// Paths captured into a checkpoint even though the repository ignores them.
    ///
    /// The one exception to "ignored files do not travel": a file a step
    /// generates, the repository ignores, and a later step needs — a `.env`
    /// being the usual one. Everything else is rebuilt by the worker that takes
    /// the execution over.
    #[serde(default)]
    pub preserve_paths: Vec<String>,
```

Przenieś je w `src/workflow/loader.rs` do modelu domenowego tą samą drogą, którą idą pozostałe
pola workspace'u.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --test checkpoint && cargo test --test schema_loading`
Expected: PASS.

- [ ] **Step 5: Document it**

Dopisz `preserve_paths` do `WORKFLOW_SCHEMA.md` w sekcji opisującej `globals.workspace`,
jednym akapitem, z przykładem `.env` i ze zdaniem, że wszystko inne worker odtwarza sam.

- [ ] **Step 6: Commit**

```bash
git add src/workflow/schema.rs src/workflow/loader.rs WORKFLOW_SCHEMA.md tests/checkpoint.rs tests/schema_loading.rs
git commit -m "feat(workflow): let a definition name the ignored paths a checkpoint keeps"
```

---

### Task 7: Hak checkpointu na granicy kroku (R4)

**Files:**
- Modify: `src/engine/runner.rs:346-352` (pole), `:390-400` (konstruktor), `:987-1000` (hak)
- Test: `tests/engine_execution.rs`

**Interfaces:**
- Consumes: `capture`, `Base`, `CaptureRequest` (Task 3); `StatePaths::checkpoint_file`,
  `checkpoint_scratch` (Task 3); `Workspace::base_commit` (Task 1).
- Produces: `Engine::with_checkpoints(self) -> Self` — używane przez Task 8.

- [ ] **Step 1: Write the failing test**

Dopisz do `tests/engine_execution.rs`:

```rust
#[tokio::test]
async fn checkpoints_are_written_at_every_step_boundary() {
    let harness = Harness::new();
    let definition = workflow("two_steps.yaml");
    let engine = harness.engine().with_checkpoints();

    let execution = engine
        .start_and_run(&definition, serde_json::json!({}), harness.workspace())
        .await
        .expect("run the workflow");

    let bundle = harness
        .store()
        .paths()
        .checkpoint_file(&execution.id);

    assert!(
        bundle.is_file(),
        "a finished execution must have left a checkpoint at {}",
        bundle.display()
    );
}

#[tokio::test]
async fn an_engine_without_checkpoints_writes_none() {
    // The default stays off in M1, so no existing test starts shelling out to
    // git on every step.
    let harness = Harness::new();
    let definition = workflow("two_steps.yaml");

    let execution = harness
        .engine()
        .start_and_run(&definition, serde_json::json!({}), harness.workspace())
        .await
        .expect("run the workflow");

    assert!(
        !harness
            .store()
            .paths()
            .checkpoint_file(&execution.id)
            .exists(),
        "checkpointing is opt-in in M1"
    );
}
```

Jeśli fikstura `two_steps.yaml` nie istnieje, użyj tej, której ten plik już używa do przebiegu
o co najmniej dwóch stanach.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test engine_execution checkpoints_are_written`
Expected: FAIL — `no method named 'with_checkpoints'`.

- [ ] **Step 3: Add the field and the builder**

W `src/engine/runner.rs`, w strukturze `Engine`, po `pause_requests`:

```rust
    checkpoints: bool,
```

Ustaw `checkpoints: false` w `Engine::new`, i dopisz obok `with_agent_defaults`:

```rust
    /// Writes a checkpoint of the workspace at every step boundary (R4).
    ///
    /// Opt-in while there is no hub to replicate to: turning it on for every
    /// engine would make each step of every existing test shell out to `git`
    /// for no gain. From M2 on, the hub's worker turns it on unconditionally.
    pub fn with_checkpoints(mut self) -> Self {
        self.checkpoints = true;
        self
    }
```

- [ ] **Step 4: Write the hook**

W `src/engine/runner.rs`, na końcu `apply_decision`, **po** `self.store.save_execution(execution)?`
(czyli po `TransitionTaken` i po zapisie `state.json`), dodaj:

```rust
        self.write_checkpoint(execution);
```

i metodę na `Engine`:

```rust
    /// Captures the workspace at a step boundary, if checkpointing is on.
    ///
    /// A failing checkpoint does not fail the step: the execution's own record
    /// is already durable, and refusing to go on because a *copy* could not be
    /// made would turn a recoverable situation into a stopped workflow. The
    /// failure is logged, and the previous checkpoint is left intact (R25).
    fn write_checkpoint(&self, execution: &WorkflowExecution) {
        if !self.checkpoints {
            return;
        }

        let base = match &execution.workspace.base_commit {
            Some(commit) => crate::workspace::Base::Commit(commit.clone()),
            None => crate::workspace::Base::None,
        };

        let paths = self.store.paths();
        let request = crate::workspace::CaptureRequest {
            work_tree: std::path::Path::new(&execution.workspace.path),
            base,
            scratch: &paths.checkpoint_scratch(&execution.id),
            bundle: &paths.checkpoint_file(&execution.id),
            preserve: &[],
        };

        if let Err(error) = crate::workspace::capture(request) {
            tracing::warn!(
                execution_id = %execution.id,
                %error,
                "could not checkpoint the workspace; the previous checkpoint is kept"
            );
        }
    }
```

`preserve` zostaje pustą listą do czasu, aż Task 6 doprowadzi `preserve_paths` do modelu
domenowego; jeśli Task 6 jest już zrobione, przekaż tu listę z definicji.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --test engine_execution`
Expected: PASS.

- [ ] **Step 6: Run the whole suite**

Run: `cargo test`
Expected: PASS — domyślnie wyłączone, więc `engine_recovery` i reszta nie zmieniają zachowania.

- [ ] **Step 7: Commit**

```bash
git add src/engine/runner.rs tests/engine_execution.rs
git commit -m "feat(engine): checkpoint the workspace at each step boundary"
```

---

### Task 8: Przejęcie i dowód równoważności (R26, R27, AE1)

**Files:**
- Modify: `src/tasks/mod.rs` (dopisz `takeover` i `TakeoverError`)
- Test: `tests/takeover.rs` *(nowy)*

**Interfaces:**
- Consumes: `reconcile` (`src/persistence/recovery.rs:156`), `materialise` (Task 4),
  `resolve_definition` (Task 2), `WorkspaceProvisioner::provision`, `Workspace::base_commit`.
- Produces:
  - `pub fn takeover(store: &ExecutionStore, provisioner: &WorkspaceProvisioner, execution_id: &str) -> Result<WorkflowExecution, TakeoverError>`
  - `pub enum TakeoverError { Persistence(PersistenceError), Workspace(WorkspaceError), Checkpoint(CheckpointError), NoCheckpoint { execution_id: String } }`

- [ ] **Step 1: Write the failing test**

Utwórz `tests/takeover.rs`:

```rust
//! AE1: taking an execution over is the same operation as recovering one.
//!
//! The proof is an equivalence: a run that is never interrupted and a run that
//! is interrupted after step k and taken over by a fresh process must reach the
//! same final state, for every k. Script actions only, so the comparison is
//! deterministic.

mod engine_support;

use engine_support::{workflow, Harness};
use narsor::domain::ExecutionStatus;

#[tokio::test]
async fn an_interrupted_run_ends_where_an_uninterrupted_one_does() {
    let reference = {
        let harness = Harness::new();
        let engine = harness.engine().with_checkpoints();
        engine
            .start_and_run(
                &workflow("three_steps.yaml"),
                serde_json::json!({}),
                harness.workspace(),
            )
            .await
            .expect("reference run")
    };

    for interrupt_after in 1..=3 {
        let harness = Harness::new();
        let engine = harness.engine().with_checkpoints();

        let mut execution = engine
            .start(
                &workflow("three_steps.yaml"),
                serde_json::json!({}),
                harness.workspace(),
            )
            .expect("start");

        engine
            .run_steps(&mut execution, interrupt_after)
            .await
            .expect("run up to the interruption");

        // The process dies here: a fresh store, a fresh engine, and a work tree
        // that is deliberately destroyed so nothing can be recovered from it.
        std::fs::remove_dir_all(&execution.workspace.path).expect("destroy the work tree");

        let taken_over = narsor::tasks::takeover(
            &harness.store(),
            &harness.provisioner(),
            &execution.id,
        )
        .expect("take the execution over");

        let finished = harness
            .engine()
            .with_checkpoints()
            .run(taken_over)
            .await
            .expect("finish after the takeover");

        assert_eq!(
            finished.status,
            ExecutionStatus::Completed,
            "takeover after step {interrupt_after} must finish the workflow"
        );
        assert_eq!(
            finished.context.variables, reference.context.variables,
            "takeover after step {interrupt_after} must reach the reference context"
        );
        assert_eq!(
            finished.current_state, reference.current_state,
            "takeover after step {interrupt_after} must reach the reference state"
        );

        // AE9: the handover mechanism pushes nothing to the product repository.
        assert!(
            harness.origin_refs().is_empty(),
            "takeover after step {interrupt_after} must leave the product repo untouched, found: {:?}",
            harness.origin_refs()
        );
    }
}
```

Test potrzebuje trzech rzeczy, których harness jeszcze nie ma — dodaj je w
`tests/engine_support/mod.rs` w stylu tego pliku:

- fikstura `three_steps.yaml`: trzy stany `script`, każdy dopisujący swoją zmienną do kontekstu
  i jeden plik do workspace'u, kończące się stanem terminalnym;
- `Harness::provisioner(&self) -> WorkspaceProvisioner` nad `self.root.join("work")`;
- `Harness::origin_refs(&self) -> Vec<String>` — `git ls-remote` do repozytorium origin fikstury,
  zwracające nazwy refów; pusty wektor, gdy fikstura nie używa repozytorium gitowego;
- `Engine::run_steps(&self, execution: &mut WorkflowExecution, steps: usize)` — pętla silnika
  zatrzymująca się po `steps` granicach kroku. Jeśli dodanie tego do `Engine` jest zbyt
  inwazyjne, osiągnij to samo, wołając `run` z workflow o `steps` stanach; wtedy `reference`
  liczy się osobno dla każdego `interrupt_after`.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test takeover`
Expected: FAIL — `cannot find function 'takeover' in module 'narsor::tasks'`.

- [ ] **Step 3: Write the implementation**

Dopisz do `src/tasks/mod.rs`:

```rust
/// Everything that can stop an execution from being taken over.
#[derive(Debug, thiserror::Error)]
pub enum TakeoverError {
    /// The store could not be read or written.
    #[error("{0}")]
    Persistence(#[from] PersistenceError),

    /// The workspace could not be provisioned.
    #[error("{0}")]
    Workspace(#[from] WorkspaceError),

    /// The checkpoint could not be restored.
    #[error("{0}")]
    Checkpoint(#[from] CheckpointError),

    /// The execution never wrote a checkpoint, so there is nothing to restore.
    #[error("execution {execution_id} has no checkpoint to take over from")]
    NoCheckpoint {
        /// The execution that was asked for.
        execution_id: String,
    },
}

/// Takes an execution over: reconcile, then rebuild the workspace from its
/// checkpoint.
///
/// This is the counterpart of [`start_task`] and the one path by which an
/// execution resumes in a process that did not start it — whether because the
/// previous one crashed, because its worker was killed, or because the work is
/// being handed to somebody else. There is deliberately no second path: the
/// work tree left behind by the previous process is **never** reused, because a
/// path that only works when the directory survived is a path a handover cannot
/// take (R26).
pub fn takeover(
    store: &ExecutionStore,
    provisioner: &WorkspaceProvisioner,
    execution_id: &str,
) -> std::result::Result<WorkflowExecution, TakeoverError> {
    // D23, unchanged: an action in flight is not replayed, it is reported.
    crate::persistence::reconcile(store, execution_id)?;

    let mut execution = store.load_execution(execution_id)?;

    let bundle = store.paths().checkpoint_file(execution_id);
    if !bundle.is_file() {
        return Err(TakeoverError::NoCheckpoint {
            execution_id: execution_id.to_string(),
        });
    }

    // The work tree the previous process left behind is discarded, not reused
    // (R26). It is scratch: everything durable is in the checkpoint, and work
    // done after the last step boundary is the loss window this design already
    // accepts. Removing it is also what lets `provision` run at all, since it
    // refuses to write into a task directory that already exists.
    let task_dir = provisioner.paths().task_dir(execution_id);
    if task_dir.exists() {
        fs::remove_dir_all(&task_dir).map_err(PersistenceError::from)?;
    }

    // Rebuild the checkout from the pinned descriptor, with this worker's own
    // credentials, then lay the delta over it.
    let provisioned = provisioner.provision(execution_id, &workspace_request(&execution)?)?;
    let work_tree = provisioned.primary().path.clone();

    let base = match &execution.workspace.base_commit {
        Some(commit) => crate::workspace::Base::Commit(commit.clone()),
        None => crate::workspace::Base::None,
    };
    if let crate::workspace::Base::Commit(commit) = &base {
        checkout_commit(&work_tree, commit)?;
    }

    crate::workspace::materialise(crate::workspace::MaterialiseRequest {
        work_tree: &work_tree,
        base,
        scratch: &store.paths().checkpoint_scratch(execution_id),
        bundle: &bundle,
    })?;

    execution.workspace.path = work_tree.display().to_string();
    store.save_execution(&execution)?;

    Ok(execution)
}
```

Dopisz dwa prywatne pomocniki w tym samym pliku:

- `fn workspace_request(execution: &WorkflowExecution) -> Result<WorkspaceRequest, TakeoverError>`
  — buduje `WorkspaceRequest` z `execution.workspace` (`repository.remote` + `git_ref` dla
  `RepositorySpec::git`, ścieżka dla `RepositorySpec::local`, a dla workspace'u bez repozytorium
  ten sam pusty katalog, który `start_task` tworzy pod `FALLBACK_WORKSPACE_NAME`);
- `fn checkout_commit(work_tree: &Path, commit: &str) -> Result<(), TakeoverError>` — `git checkout`
  na przypięty sha, bo `provision` klonuje na gałąź, a delta jest mierzona od commita.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --test takeover -- --nocapture`
Expected: PASS dla wszystkich trzech punktów przerwania.

- [ ] **Step 5: Run the whole suite**

Run: `cargo test`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/tasks/mod.rs tests/takeover.rs tests/engine_support/mod.rs tests/fixtures
git commit -m "feat(tasks): take an execution over by rebuilding it from its checkpoint"
```

---

### Task 9: Dokumentacja i decyzje

**Files:**
- Modify: `ARCHITECTURE_DECISIONS.md`
- Modify: `README.md`
- Modify: `DOMAIN_MODEL.md`

**Interfaces:**
- Consumes: wszystko powyżej.
- Produces: nic w kodzie.

- [ ] **Step 1: Record the decisions**

Dopisz do tabeli w `ARCHITECTURE_DECISIONS.md` wiersze **D48–D50**, w stylu D47 (pełne zdanie
w kolumnie decyzji, alternatywa, status, konsekwencje ze wskaźnikiem do
`docs/brainstorms/2026-09-25-rozproszony-narsor-requirements.md`):

- **D48 — Wykonanie jest samowystarczalne**: niesie przypiętą definicję i bazowy commit;
  alternatywa: wyszukiwanie po nazwie w katalogu.
- **D49 — Checkpoint workspace'u jako git bundle delty**: alternatywy odrzucone —
  gałęzie WIP na repozytorium produktowym (CI, uprawnienia, brak obsługi workflow bez gita)
  i dedykowane repozytorium stanu (zależność od forge'a).
- **D50 — Materializacja wyłącznie z checkpointu**: alternatywa: odzyskiwanie ocalałego
  katalogu; odrzucona, bo działa tylko przy tej samej maszynie i tworzy drugą ścieżkę.

Oznacz **D2** jako zastąpioną przez D48–D50 w części dotyczącej związania wykonania z procesem.
**D35** i **D14** zostają nietknięte w tym planie — przewraca je M2 i M3, nie M1.

- [ ] **Step 2: Document the layout**

W `README.md`, w sekcji „The on-disk layout", dopisz trzy nowe pliki wykonania
(`definition.yaml`, `checkpoint.bundle`, `checkpoint.git/`) z jednym zdaniem o każdym.
W `DOMAIN_MODEL.md` §2.8 dopisz akapit o checkpoincie jako delcie względem przypiętej bazy.

- [ ] **Step 3: Commit**

```bash
git add ARCHITECTURE_DECISIONS.md README.md DOMAIN_MODEL.md
git commit -m "docs: record the self-contained execution and its checkpoint"
```

---

## Czego ten plan świadomie nie robi

- **Nie włącza checkpointowania domyślnie.** Opt-in do M2, żeby istniejące testy nie zaczęły
  uruchamiać `git` na każdym kroku (Global Constraints).
- **Nie dotyka sieci, dzierżawy ani przypisania.** To M2 i M3.
- **Nie emituje zdarzenia porzucenia sesji agenta** (R28). W M1 przejęcie jest lokalne, więc stan
  harnessu w katalogu domowym przeżywa; sens ma to dopiero przy przejęciu międzymaszynowym.
- **Nie obsługuje repozytoriów wtórnych w delcie.** Są tylko do odczytu (D15), więc worker
  docelowy klonuje je od nowa; delta dotyczy wyłącznie repozytorium głównego.
