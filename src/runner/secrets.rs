//! Handing mounted credentials to the tools that need them (R19).
//!
//! The backend mounts every credential of a task read-only, one file per
//! secret reference *name*, in a directory outside task storage (for
//! Kubernetes, a projected secret volume). The runner never copies a value
//! anywhere: not into the workspace, the Engine state, the private HOME or a
//! log. It only makes each value reachable the way its tool expects
//! ([`SecretDelivery`]):
//!
//! * **`env`** — the value becomes an environment variable of the runner
//!   process, which every action's child process inherits. This is the one
//!   place a value leaves its file, because that is what the tool asked for;
//!   [`ProcessEnvironment`] hands it to the caller that owns the process
//!   environment (the CLI entry point) rather than setting it from library
//!   code.
//! * **`file`** — a symbolic link at the requested path below the private
//!   HOME points at the mounted file. The link lives on task storage; the
//!   value does not. A link left by an earlier incarnation is replaced, so a
//!   remounted secret is always the one used.
//!
//! The Git clone credential (R39) is used the same way: by path, through a
//! credential helper that reads the file itself
//! ([`crate::workspace::clone_pinned_commit`]).

use std::fs;
use std::path::{Path, PathBuf};

use crate::domain::{ResolvedEnvironment, SecretDelivery, SecretReference, SecretValue};

use super::RunnerError;

/// Environment a runner process must set before it starts any work.
#[derive(Debug, Default)]
pub struct ProcessEnvironment {
    /// The private HOME on task storage (R17).
    pub home: PathBuf,
    /// `env`-delivered secrets, by variable name.
    pub secrets: Vec<(String, SecretValue)>,
}

impl ProcessEnvironment {
    /// Every variable to set, the secrets last.
    pub fn variables(&self) -> impl Iterator<Item = (&str, &str)> {
        std::iter::once(("HOME", self.home.to_str().unwrap_or_default())).chain(
            self.secrets
                .iter()
                .map(|(name, value)| (name.as_str(), value.expose())),
        )
    }
}

/// The mounted secrets of a task.
#[derive(Debug, Clone)]
pub struct MountedSecrets {
    dir: Option<PathBuf>,
}

impl MountedSecrets {
    /// Secrets mounted under `dir`, or none at all.
    pub fn new(dir: Option<PathBuf>) -> Self {
        Self { dir }
    }

    /// The mounted file of `reference`, checked to exist.
    pub fn file(&self, reference: &SecretReference) -> Result<PathBuf, RunnerError> {
        let missing = |detail: String| RunnerError::Secret {
            name: reference.name.clone(),
            detail,
        };
        if !is_plain_name(&reference.name) {
            return Err(missing(
                "a secret reference name must be one plain path component".to_string(),
            ));
        }
        let Some(dir) = &self.dir else {
            return Err(missing(
                "no secrets directory was mounted (--secrets-dir)".to_string(),
            ));
        };
        let path = dir.join(&reference.name);
        if !path.is_file() {
            return Err(missing(format!("'{}' is not mounted", path.display())));
        }
        Ok(path)
    }

    /// Makes every secret of `environment` reachable for its tool, below
    /// `home`, and returns what the process environment must hold.
    ///
    /// Fails before any work when a referenced secret is not mounted: a
    /// tool that finds no credential half-way through a task is a worse
    /// failure than a task that never starts.
    pub fn materialize(
        &self,
        environment: &ResolvedEnvironment,
        home: &Path,
    ) -> Result<ProcessEnvironment, RunnerError> {
        let mut process = ProcessEnvironment {
            home: home.to_path_buf(),
            secrets: Vec::new(),
        };
        for reference in &environment.secrets {
            if let Some(problem) = reference.delivery.problem() {
                return Err(RunnerError::Secret {
                    name: reference.name.clone(),
                    detail: problem,
                });
            }
            let file = self.file(reference)?;
            match &reference.delivery {
                SecretDelivery::Env(name) => {
                    let value = fs::read_to_string(&file)
                        .map_err(|source| RunnerError::io(&file, source))?;
                    let value = value.strip_suffix('\n').unwrap_or(&value).to_string();
                    process
                        .secrets
                        .push((name.clone(), SecretValue::new(value)));
                }
                SecretDelivery::File(relative) => link_into_home(home, relative, &file)?,
            }
        }
        Ok(process)
    }
}

/// Points `{home}/{relative}` at the mounted `target`.
fn link_into_home(home: &Path, relative: &str, target: &Path) -> Result<(), RunnerError> {
    let link = home.join(relative);
    if let Some(parent) = link.parent() {
        fs::create_dir_all(parent).map_err(|source| RunnerError::io(parent, source))?;
    }
    match fs::symlink_metadata(&link) {
        Ok(metadata) if metadata.is_dir() => {
            return Err(RunnerError::Secret {
                name: relative.to_string(),
                detail: format!("'{}' is a directory in the private HOME", link.display()),
            })
        }
        // A link of an earlier incarnation, or a file a tool wrote over the
        // path reserved for the credential: either way the mounted secret
        // is what belongs there now.
        Ok(_) => fs::remove_file(&link).map_err(|source| RunnerError::io(&link, source))?,
        Err(_) => {}
    }
    std::os::unix::fs::symlink(target, &link).map_err(|source| RunnerError::io(&link, source))
}

fn is_plain_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && !name.contains('/')
        && !name.contains('\0')
        && name.len() <= 255
}
