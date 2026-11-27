//! `narsor-migrate runtime-catalog`: plans the move of an old runtime
//! catalogue into the runtime directory of the catalogue. It changes
//! nothing; `dist/install.sh` does the move with `mv -n`.
//!
//! ```text
//! {catalog}/runtime.yaml  ->  {catalog}/runtime/runtime.yaml
//! {catalog}/components/   ->  {catalog}/runtime/components/
//! ```
//!
//! The command resolves the catalogue as `narsor-server` does (bash cannot),
//! refuses while a program of the installation runs, refuses when the runtime
//! directory already holds what would move (both layouts at once), and reads
//! `runtime.yaml` for the script warning. A bare-string script outside
//! `components/` is reported as a warning, because it is not found after the
//! move until the operator moves it too.
//!
//! Output, one line each, fields separated by a tab:
//!
//! ```text
//! move<TAB>{from}<TAB>{to}     in the order to run them: scripts first
//! warning<TAB>{text}           about a script outside components/
//! ```
//!
//! A rerun after a crash between the two moves finds the old file, no old
//! `components/` and the new `runtime/components/`, and plans the file only.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use narsor_shared::config::Resolved;
use narsor_store::paths::CatalogPaths;

use crate::{refusal, Installation, EXIT_FAILED, EXIT_REFUSED, EXIT_RUNNING};

/// The catalogue whose runtime catalogue moves, and the locks no program
/// may hold while it does.
#[derive(Debug, Clone)]
pub struct RuntimeCatalogMove {
    /// The catalogue.
    pub catalog: CatalogPaths,
    /// The locks of the installation's programs.
    pub locks: Vec<refusal::Lock>,
}

impl RuntimeCatalogMove {
    /// The catalogue `config` resolves (scope of `narsor-server`), with the
    /// locks of the same installation.
    pub fn of(config: &Resolved) -> Self {
        Self {
            catalog: CatalogPaths::new(config.catalog_dir()),
            locks: Installation::of(config).locks(),
        }
    }
}

/// What a run found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// No runtime catalogue at the old place.
    NothingToMove,
    /// The moves to make, in order, and the scripts not found after them.
    Plan {
        /// From and to of each move.
        moves: Vec<(PathBuf, PathBuf)>,
        /// One line per script path outside `components/`.
        warnings: Vec<String>,
    },
}

/// Why a run stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// A program of the installation runs.
    Running(String),
    /// The runtime directory already holds what would move.
    Collision(String),
    /// The check could not run.
    Failed(String),
}

impl Failure {
    /// The exit status of the command for this failure.
    pub fn exit_code(&self) -> i32 {
        match self {
            Failure::Running(_) => EXIT_RUNNING,
            Failure::Collision(_) => EXIT_REFUSED,
            Failure::Failed(_) => EXIT_FAILED,
        }
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::Running(reason) => write!(f, "refused, nothing to do now:\n{reason}"),
            Failure::Collision(reason) => write!(f, "refused: {reason}"),
            Failure::Failed(reason) => write!(f, "{reason}"),
        }
    }
}

/// One move: from, to.
type Move = (PathBuf, PathBuf);

/// Plans the move of the old runtime catalogue of `target` (see the module
/// documentation). `out` gets the plan lines. Nothing changes on disk.
pub fn run(target: &RuntimeCatalogMove, out: &mut dyn Write) -> Result<Outcome, Failure> {
    let failed = |error: io::Error| Failure::Failed(error.to_string());
    let catalog = &target.catalog;
    let old_file = catalog.legacy_runtime_file();
    if !exists(&old_file) {
        writeln!(
            out,
            "No runtime catalogue at the old place {}; nothing to move.",
            old_file.display()
        )
        .map_err(failed)?;
        return Ok(Outcome::NothingToMove);
    }
    let moves = plan(catalog)?;

    let checks = refusal::Checks {
        locks: target.locks.clone(),
        ..refusal::Checks::default()
    };
    // The locks are released at once: the installer moves later and checks
    // each target again right before its move.
    drop(
        refusal::check(&checks, &NoContainers, true).map_err(|refusal| {
            if refusal.only_held_locks {
                Failure::Running(refusal.reason)
            } else {
                Failure::Failed(refusal.reason)
            }
        })?,
    );
    let warnings = outside_components(&old_file, &catalog.runtime_dir());

    for (from, to) in &moves {
        writeln!(out, "move\t{}\t{}", from.display(), to.display()).map_err(failed)?;
    }
    for warning in &warnings {
        writeln!(out, "warning\t{warning}").map_err(failed)?;
    }
    Ok(Outcome::Plan { moves, warnings })
}

/// The moves for a catalogue with an old runtime file, scripts first, or
/// the collision that stops the move.
fn plan(catalog: &CatalogPaths) -> Result<Vec<Move>, Failure> {
    let new_file = catalog.runtime_file();
    for existing in [new_file.clone(), new_file.with_extension("yml")] {
        if exists(&existing) {
            return Err(Failure::Collision(format!(
                "the runtime catalogue exists at the old place {} and at {}. Keep one: move \
                 what you need into {}, then delete {}",
                catalog.legacy_runtime_file().display(),
                existing.display(),
                existing.display(),
                catalog.legacy_runtime_file().display()
            )));
        }
    }
    let old_components = catalog.legacy_components_dir();
    let new_components = catalog.runtime_components_dir();
    let mut moves = Vec::new();
    if exists(&old_components) {
        if exists(&new_components) {
            return Err(Failure::Collision(format!(
                "both {} and {} exist. Merge them into {}, delete {}, then run the command \
                 again",
                old_components.display(),
                new_components.display(),
                new_components.display(),
                old_components.display()
            )));
        }
        moves.push((old_components, new_components));
    }
    moves.push((catalog.legacy_runtime_file(), new_file));
    Ok(moves)
}

/// One line per bare-string `install` or `version` script under
/// `task_image.components` that is not in `components/`: after the move it
/// is looked up in `runtime_dir`, where it is not. Best effort: a file
/// that does not parse yields nothing here.
fn outside_components(file: &Path, runtime_dir: &Path) -> Vec<String> {
    let Some(document) = std::fs::read_to_string(file)
        .ok()
        .and_then(|text| serde_yaml::from_str::<serde_yaml::Value>(&text).ok())
    else {
        return Vec::new();
    };
    let Some(components) = document
        .get("task_image")
        .and_then(|image| image.get("components"))
        .and_then(serde_yaml::Value::as_sequence)
    else {
        return Vec::new();
    };
    let mut warnings = Vec::new();
    for (index, component) in components.iter().enumerate() {
        for field in ["install", "version"] {
            let Some(path) = component.get(field).and_then(serde_yaml::Value::as_str) else {
                continue;
            };
            if !path.starts_with("components/") {
                warnings.push(format!(
                    "task_image.components[{index}].{field}: script '{path}' is not in \
                     components/. Script paths now resolve from {dir}, so move the script to \
                     {target} or make it inline ({{ inline: ... }}); until then a Docker-mode \
                     task does not start",
                    dir = runtime_dir.display(),
                    target = runtime_dir.join(path).display()
                ));
            }
        }
    }
    warnings
}

fn exists(path: &Path) -> bool {
    path.symlink_metadata().is_ok()
}

/// The lock check asks Docker nothing here: it checks no workloads.
struct NoContainers;

impl refusal::ContainerProbe for NoContainers {
    fn running(&self, _name: &str) -> Result<bool, String> {
        Ok(false)
    }
}
