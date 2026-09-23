//! The repository registry and its single JSON file.
//!
//! The whole catalogue lives in one document, `{root}/repositories.json`. It is
//! small by nature — a handful of repositories a team runs workflows against —
//! so a directory of one file per entry would buy nothing and cost a partially
//! written catalogue. One document also makes every mutation a single atomic
//! rename: readers see the registry before or after a change, never halfway
//! through one.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::domain::{RepositoryInfo, RepositoryReference};
use crate::persistence::{read_json, write_json_atomic, PersistenceError};
use crate::workspace::provisioner::{check_remote, is_usable_directory_name};

/// Name of the registry file, directly under the orchestrator root.
pub const REPOSITORIES_FILE: &str = "repositories.json";

/// Result alias for every fallible registry operation.
pub type Result<T> = std::result::Result<T, RegistryError>;

/// Errors produced by the repository registry.
///
/// Every variant names the repository id it is about, and the ones that are
/// about the catalogue as a whole also name the file: a registry failure is
/// diagnosed by looking at one entry in one file on disk.
#[derive(Debug, Error)]
pub enum RegistryError {
    /// The registry file could not be read or written.
    #[error("repository registry '{path}' is unusable: {source}")]
    Storage {
        /// The registry file.
        path: PathBuf,
        /// The underlying persistence error.
        #[source]
        source: PersistenceError,
    },

    /// An entry with this id is already registered.
    ///
    /// Never resolved by overwriting: a workflow already names this id, and
    /// silently repointing it at another remote would change what that workflow
    /// checks out. Use [`RepositoryRegistry::update`] to change an entry on
    /// purpose.
    #[error("repository '{id}' is already registered in '{path}'")]
    DuplicateId {
        /// The id that collided.
        id: String,
        /// The registry file holding the existing entry.
        path: PathBuf,
    },

    /// No entry with this id is registered.
    #[error("repository '{id}' is not registered in '{path}'")]
    NotFound {
        /// The id that was looked up.
        id: String,
        /// The registry file that was searched.
        path: PathBuf,
    },

    /// An id that cannot name the directory the checkout lands in.
    #[error("repository id '{id}' is not a usable directory name")]
    InvalidId {
        /// The offending id.
        id: String,
    },

    /// A remote `git` would not read as a plain URL.
    ///
    /// The check is the provisioner's own, so a remote that the registry
    /// accepts is a remote the provisioner will accept too.
    #[error("repository '{id}' cannot be registered: {source}")]
    InvalidRemote {
        /// The entry that was rejected.
        id: String,
        /// The provisioner's verdict, naming the remote and the reason.
        #[source]
        source: crate::workspace::WorkspaceError,
    },
}

/// One registered repository.
///
/// The `id` is the key a workflow writes in
/// `globals.workspace.repository`; it doubles as the directory name the
/// checkout lands in, which is why it is validated as one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryEntry {
    /// Stable identifier, unique within the registry.
    pub id: String,

    /// Human-readable name, for display.
    pub name: String,

    /// Where the repository is hosted: `github`, `gitlab`, `local`, …
    pub provider: String,

    /// Anything `git clone` accepts: a URL or an absolute local path.
    pub remote: String,

    /// Branch checked out when a workflow declares no `ref` of its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,

    /// Free-form note about what the repository is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Guesses the provider from the shape of a remote.
///
/// Only a default for an omitted provider: the field is free text and getting
/// it wrong costs a display label, never a clone. It lives here rather than in
/// a caller so the command line and the interface label the same remote the
/// same way — registering one repository two ways should not describe it two
/// ways.
pub fn infer_provider(remote: &str) -> &'static str {
    let lowered = remote.to_ascii_lowercase();
    if lowered.contains("github.") {
        "github"
    } else if lowered.contains("gitlab.") {
        "gitlab"
    } else if lowered.starts_with('/') || lowered.starts_with('.') || lowered.starts_with("file://")
    {
        "local"
    } else {
        "git"
    }
}

impl RepositoryEntry {
    /// A minimal entry: the id stands in for the name, the provider is `git`.
    pub fn new(id: impl Into<String>, remote: impl Into<String>) -> Self {
        let id = id.into();
        let remote = remote.into();
        Self {
            name: id.clone(),
            id,
            provider: infer_provider(&remote).to_string(),
            remote,
            default_branch: None,
            description: None,
        }
    }

    /// Sets the display name.
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// Sets the provider.
    pub fn with_provider(mut self, provider: impl Into<String>) -> Self {
        self.provider = provider.into();
        self
    }

    /// Sets the branch used when a workflow declares no `ref`.
    pub fn with_default_branch(mut self, branch: impl Into<String>) -> Self {
        self.default_branch = Some(branch.into());
        self
    }

    /// Sets the free-form description.
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Checks the entry with the provisioner's own rules.
    ///
    /// Called by every mutating operation, and public so that a caller which
    /// builds an entry interactively (the TUI) can report the same verdict
    /// before it tries to store anything.
    pub fn validate(&self) -> Result<()> {
        if !is_usable_directory_name(self.id.trim()) || self.id.trim() != self.id {
            return Err(RegistryError::InvalidId {
                id: self.id.clone(),
            });
        }
        check_remote(&self.id, &self.remote).map_err(|source| RegistryError::InvalidRemote {
            id: self.id.clone(),
            source,
        })
    }

    /// The context-level reference to this repository.
    pub fn to_reference(&self) -> RepositoryReference {
        RepositoryReference {
            id: self.id.clone(),
            name: self.name.clone(),
            provider: self.provider.clone(),
            remote: self.remote.clone(),
            default_branch: self.default_branch.clone(),
        }
    }

    /// The workspace-level description of this repository.
    pub fn to_info(&self) -> RepositoryInfo {
        RepositoryInfo {
            id: self.id.clone(),
            name: self.name.clone(),
            provider: self.provider.clone(),
            remote: self.remote.clone(),
        }
    }
}

impl From<&RepositoryEntry> for RepositoryReference {
    fn from(entry: &RepositoryEntry) -> Self {
        entry.to_reference()
    }
}

impl From<&RepositoryEntry> for RepositoryInfo {
    fn from(entry: &RepositoryEntry) -> Self {
        entry.to_info()
    }
}

/// The on-disk document. Wrapped in an object so a later version can add
/// fields beside `repositories` without rewriting every file in the field.
#[derive(Debug, Default, Serialize, Deserialize)]
struct RegistryFile {
    #[serde(default)]
    repositories: Vec<RepositoryEntry>,
}

/// The catalogue of repositories workflows may name by id.
///
/// Holds no state of its own: every operation reads the file and every mutation
/// writes it back atomically, so two processes sharing a root never disagree
/// about what is registered — the last writer wins the whole document, and no
/// reader ever sees a half-written one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryRegistry {
    path: PathBuf,
}

impl RepositoryRegistry {
    /// Opens the registry stored under `root`.
    ///
    /// Nothing is created or read here: a root without a `repositories.json` is
    /// simply an empty registry.
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            path: root.as_ref().join(REPOSITORIES_FILE),
        }
    }

    /// The file the registry is stored in.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every registered repository, ordered by id.
    ///
    /// A missing file is an empty registry, never an error: not having
    /// registered anything yet is the normal state of a fresh root.
    pub fn list(&self) -> Result<Vec<RepositoryEntry>> {
        Ok(self.load()?.repositories)
    }

    /// The entry with this id, or `None` when nothing is registered under it.
    pub fn get(&self, id: &str) -> Result<Option<RepositoryEntry>> {
        Ok(self
            .load()?
            .repositories
            .into_iter()
            .find(|entry| entry.id == id))
    }

    /// Whether an entry with this id is registered.
    pub fn contains(&self, id: &str) -> Result<bool> {
        Ok(self.get(id)?.is_some())
    }

    /// Registers a new repository, refusing an id that is already taken.
    ///
    /// On rejection the stored entry is left exactly as it was — the file is
    /// not touched at all.
    pub fn add(&self, entry: RepositoryEntry) -> Result<()> {
        entry.validate()?;

        let mut file = self.load()?;
        if file.repositories.iter().any(|stored| stored.id == entry.id) {
            return Err(RegistryError::DuplicateId {
                id: entry.id,
                path: self.path.clone(),
            });
        }
        file.repositories.push(entry);
        self.save(file)
    }

    /// Stores an entry, replacing one with the same id if there is one.
    pub fn update(&self, entry: RepositoryEntry) -> Result<()> {
        entry.validate()?;

        let mut file = self.load()?;
        match file
            .repositories
            .iter_mut()
            .find(|stored| stored.id == entry.id)
        {
            Some(stored) => *stored = entry,
            None => file.repositories.push(entry),
        }
        self.save(file)
    }

    /// Removes the entry with this id, reporting an id that was never there.
    ///
    /// Removing nothing is an error rather than a silent success: an operator
    /// who mistypes an id would otherwise be told the repository is gone while
    /// it is still registered.
    pub fn remove(&self, id: &str) -> Result<RepositoryEntry> {
        let mut file = self.load()?;
        let index = file
            .repositories
            .iter()
            .position(|entry| entry.id == id)
            .ok_or_else(|| RegistryError::NotFound {
                id: id.to_string(),
                path: self.path.clone(),
            })?;
        let removed = file.repositories.remove(index);
        self.save(file)?;
        Ok(removed)
    }

    /// Reads the document, treating a missing file as an empty registry.
    fn load(&self) -> Result<RegistryFile> {
        let mut file: RegistryFile = match read_json(&self.path) {
            Ok(file) => file,
            Err(PersistenceError::Io { source, .. }) if source.kind() == ErrorKind::NotFound => {
                RegistryFile::default()
            }
            Err(source) => {
                return Err(RegistryError::Storage {
                    path: self.path.clone(),
                    source,
                })
            }
        };
        // Ordering is part of the contract: `repo list` and the TUI must show
        // the same sequence whatever order entries were added in.
        file.repositories.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(file)
    }

    /// Writes the document back, sorted, through the atomic writer.
    fn save(&self, mut file: RegistryFile) -> Result<()> {
        file.repositories.sort_by(|a, b| a.id.cmp(&b.id));
        write_json_atomic(&self.path, &file).map_err(|source| RegistryError::Storage {
            path: self.path.clone(),
            source,
        })
    }
}
