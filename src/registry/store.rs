//! The repository registry and its directory of YAML files.
//!
//! One file per repository, `{catalog}/repositories/{id}.yaml`, and the file
//! name *is* the identifier. A single document would be smaller and would give
//! every mutation one atomic rename, which is the right trade for one writer
//! and the wrong one for several: entries do not depend on each other, so the
//! guarantee "the whole catalogue changes at once" buys nothing, while the
//! read-modify-write cycle it forces loses an entry whenever two processes
//! register different repositories at the same time.
//!
//! A directory turns the properties this layer actually wants into properties
//! of the filesystem. Uniqueness of an identifier is `link(2)` returning
//! `EEXIST`. Isolation between entries is two different paths. The blast
//! radius of a corrupt file is one repository. And the layout maps onto a
//! ConfigMap one key at a time.
//!
//! # No operation reads an entry it was not asked about
//!
//! [`add`](RepositoryRegistry::add), [`get`](RepositoryRegistry::get),
//! [`update`](RepositoryRegistry::update) and
//! [`remove`](RepositoryRegistry::remove) touch exactly one path and never
//! enumerate the directory. [`list`](RepositoryRegistry::list) is the only one
//! that reads the catalogue as a whole, and it is the only one that survives a
//! file it cannot parse.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::domain::{RepositoryInfo, RepositoryReference};
use crate::persistence::{write_atomic, write_atomic_exclusive, PersistenceError};
use crate::workspace::provisioner::{check_remote, is_usable_directory_name};

pub use crate::persistence::paths::REPOSITORIES_DIR;

/// The file extensions a registry entry may use, in lookup order.
///
/// Both spellings, for the same reason the configuration cascade accepts both:
/// a `.yml` file silently ignored is a repository that mysteriously is not
/// registered.
pub const EXTENSIONS: [&str; 2] = ["yaml", "yml"];

/// The extension a new entry is written with.
pub const DEFAULT_EXTENSION: &str = "yaml";

/// Result alias for every fallible registry operation.
pub type Result<T> = std::result::Result<T, RegistryError>;

/// Errors produced by the repository registry.
///
/// Every variant names the repository id it is about and the file it is about:
/// a registry failure is diagnosed by looking at one file on disk, and after
/// the split into a directory that file is a different one for every entry.
#[derive(Debug, Error)]
pub enum RegistryError {
    /// An entry could not be read or written.
    #[error("repository registry '{path}' is unusable: {source}")]
    Storage {
        /// The file or directory that failed.
        path: PathBuf,
        /// The underlying persistence error.
        #[source]
        source: PersistenceError,
    },

    /// An entry with this id is already registered.
    ///
    /// Reported by the filesystem refusing to link over an existing file, not
    /// by a prior read of the directory: the check and the write are one
    /// operation, so two processes registering the same id cannot both win.
    ///
    /// Never resolved by overwriting: a workflow already names this id, and
    /// silently repointing it at another remote would change what that
    /// workflow checks out. Use [`RepositoryRegistry::update`] to change an
    /// entry on purpose.
    #[error("repository '{id}' is already registered in '{path}'")]
    DuplicateId {
        /// The id that collided.
        id: String,
        /// The file holding the existing entry.
        path: PathBuf,
    },

    /// No entry with this id is registered.
    #[error("repository '{id}' is not registered in '{path}'")]
    NotFound {
        /// The id that was looked up.
        id: String,
        /// The directory that was searched.
        path: PathBuf,
    },

    /// An id that cannot name the file the entry is stored in, nor the
    /// directory the checkout lands in.
    #[error("repository id '{id}' is not a usable file name")]
    InvalidId {
        /// The offending id.
        id: String,
    },

    /// A stored entry could not be read as one.
    ///
    /// Distinct from [`RegistryError::Storage`] because the remedy is
    /// different: the file is there and readable, and what is wrong is its
    /// contents.
    #[error("repository entry '{path}' cannot be read: {message}")]
    Corrupt {
        /// The offending file.
        path: PathBuf,
        /// What is wrong with it.
        message: String,
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
/// The `id` is the key a workflow writes in `globals.workspace.repository`; it
/// doubles as the name of the file the entry is stored in and as the directory
/// name the checkout lands in, which is why it is validated as one.
///
/// It is deliberately **not** serialized. The file name is the identifier, so
/// writing it inside the file too would create a second place for it to be
/// wrong: a copied file would claim to be the repository it was copied from,
/// and every read would have to decide which of the two to believe. Reading
/// fills the field in from the file name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryEntry {
    /// Stable identifier, unique within the registry. Carried by the file name.
    #[serde(skip)]
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
        validate_id(&self.id)?;
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

/// An entry that is on disk but could not be read as one.
///
/// Carried rather than logged, because the caller decides what to do with it:
/// `repo list` prints the entries it has and then fails, while a caller after
/// one specific id fails outright.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryFailure {
    /// The file that could not be read.
    pub path: PathBuf,
    /// Why not, in one line.
    pub reason: String,
}

/// What reading the whole catalogue yielded.
///
/// Both halves, rather than one or the other, for the reason `config show`
/// keeps both: an operator holding a registry with one bad file needs to see
/// the repositories that *are* registered, and hiding them would make the
/// listing lie. Returning `Err` on the first bad file would do exactly that.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RegistryListing {
    /// Every entry that loaded, ordered by id.
    pub entries: Vec<RepositoryEntry>,
    /// Every file that did not, ordered by path.
    pub failures: Vec<RegistryFailure>,
}

impl RegistryListing {
    /// Whether every file in the directory loaded.
    pub fn is_ok(&self) -> bool {
        self.failures.is_empty()
    }

    /// Whether the registry holds nothing at all.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.failures.is_empty()
    }
}

/// The catalogue of repositories workflows may name by id.
///
/// Holds no state of its own: every operation goes to the filesystem, so two
/// processes sharing a catalogue never disagree about what is registered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryRegistry {
    dir: PathBuf,
}

impl RepositoryRegistry {
    /// Opens the registry stored under `root`.
    ///
    /// Nothing is created or read here: a root without a `repositories`
    /// directory is simply an empty registry.
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            dir: root.as_ref().join(REPOSITORIES_DIR),
        }
    }

    /// The directory the entries are stored in.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The file one entry is stored in, whether or not it exists.
    pub fn entry_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.{DEFAULT_EXTENSION}"))
    }

    /// Every registered repository, ordered by id, beside every file that
    /// could not be read as one.
    ///
    /// A missing directory is an empty registry, never an error: not having
    /// registered anything yet is the normal state of a fresh catalogue.
    pub fn list(&self) -> Result<RegistryListing> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(source) if source.kind() == ErrorKind::NotFound => {
                return Ok(RegistryListing::default())
            }
            Err(source) => {
                return Err(RegistryError::Storage {
                    path: self.dir.clone(),
                    source: PersistenceError::io(&self.dir, source),
                })
            }
        };

        let mut listing = RegistryListing::default();
        for entry in entries {
            let entry = entry.map_err(|source| RegistryError::Storage {
                path: self.dir.clone(),
                source: PersistenceError::io(&self.dir, source),
            })?;
            let path = entry.path();

            // Anything that is not a definition file is not the registry's
            // business: a README beside the entries, or a temporary file left
            // by a write that was interrupted, is not a broken repository.
            if !path.is_file() || !has_entry_extension(&path) {
                continue;
            }

            match self.read_entry(&path) {
                Ok(entry) => listing.entries.push(entry),
                Err(reason) => listing.failures.push(RegistryFailure { path, reason }),
            }
        }

        // Ordering is part of the contract: `repo list` and the TUI must show
        // the same sequence whatever order the directory happens to yield.
        listing.entries.sort_by(|a, b| a.id.cmp(&b.id));
        listing.failures.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(listing)
    }

    /// The entry with this id, or `None` when nothing is registered under it.
    ///
    /// An entry that is there but unreadable is an error rather than a `None`.
    /// A workflow names a repository by id, so answering "not registered" for
    /// a repository that *is* registered would send it to the raw-remote
    /// fallback and clone something else entirely.
    ///
    /// A lookup key that could not name a file is simply *absent*, not
    /// invalid. This is not the same judgement as
    /// [`add`](Self::add) makes about the same string, and the difference is
    /// the caller: `add` is an operator naming a repository, while this is
    /// asked about every expanded `globals.workspace.repository` value — most
    /// of which are raw git remotes that were never meant to be ids. Refusing
    /// them here would break the fallback that lets a workflow name a URL
    /// directly.
    pub fn get(&self, id: &str) -> Result<Option<RepositoryEntry>> {
        if validate_id(id).is_err() {
            return Ok(None);
        }
        for extension in EXTENSIONS {
            let path = self.dir.join(format!("{id}.{extension}"));
            match self.read_entry(&path) {
                Ok(entry) => return Ok(Some(entry)),
                Err(_) if !path.exists() => continue,
                Err(message) => return Err(RegistryError::Corrupt { path, message }),
            }
        }
        Ok(None)
    }

    /// Whether an entry with this id is registered.
    pub fn contains(&self, id: &str) -> Result<bool> {
        Ok(self.get(id)?.is_some())
    }

    /// Registers a new repository, refusing an id that is already taken.
    ///
    /// The refusal comes from the filesystem, not from a prior look at the
    /// directory, so there is no window between the check and the write. On
    /// rejection the stored entry is left exactly as it was — the file is not
    /// opened at all.
    pub fn add(&self, entry: RepositoryEntry) -> Result<()> {
        entry.validate()?;

        let path = self.entry_path(&entry.id);
        let bytes = self.serialize(&entry, &path)?;
        let written = write_atomic_exclusive(&path, &bytes)
            .map_err(|source| self.storage_error(&path, source))?;

        if written {
            Ok(())
        } else {
            Err(RegistryError::DuplicateId { id: entry.id, path })
        }
    }

    /// Stores an entry, replacing one with the same id if there is one.
    pub fn update(&self, entry: RepositoryEntry) -> Result<()> {
        entry.validate()?;

        let path = self.entry_path(&entry.id);
        let bytes = self.serialize(&entry, &path)?;
        write_atomic(&path, &bytes).map_err(|source| self.storage_error(&path, source))
    }

    /// Removes the entry with this id, reporting an id that was never there.
    ///
    /// Removing nothing is an error rather than a silent success: an operator
    /// who mistypes an id would otherwise be told the repository is gone while
    /// it is still registered.
    pub fn remove(&self, id: &str) -> Result<RepositoryEntry> {
        validate_id(id)?;
        let removed = self.get(id)?.ok_or_else(|| RegistryError::NotFound {
            id: id.to_string(),
            path: self.dir.clone(),
        })?;

        for extension in EXTENSIONS {
            let path = self.dir.join(format!("{id}.{extension}"));
            match std::fs::remove_file(&path) {
                Ok(()) => return Ok(removed),
                Err(source) if source.kind() == ErrorKind::NotFound => continue,
                Err(source) => {
                    return Err(self.storage_error(&path, PersistenceError::io(&path, source)))
                }
            }
        }

        Err(RegistryError::NotFound {
            id: id.to_string(),
            path: self.dir.clone(),
        })
    }

    /// Reads one entry file, filling the id in from the file name.
    ///
    /// The `Err` is a one-line reason rather than a [`RegistryError`] because
    /// both callers wrap it differently: [`list`](Self::list) collects it as a
    /// named failure, [`get`](Self::get) raises it.
    fn read_entry(&self, path: &Path) -> std::result::Result<RepositoryEntry, String> {
        let id = entry_id(path).ok_or_else(|| "the file name is not a usable id".to_string())?;
        let contents = std::fs::read_to_string(path).map_err(|source| source.to_string())?;
        let mut entry: RepositoryEntry =
            serde_yaml::from_str(&contents).map_err(|source| source.to_string())?;
        entry.id = id;
        Ok(entry)
    }

    /// Renders one entry as the YAML that is stored for it.
    fn serialize(&self, entry: &RepositoryEntry, path: &Path) -> Result<Vec<u8>> {
        serde_yaml::to_string(entry)
            .map(String::into_bytes)
            .map_err(|source| RegistryError::Corrupt {
                path: path.to_path_buf(),
                message: source.to_string(),
            })
    }

    /// Wraps a persistence failure, naming the file it happened on.
    fn storage_error(&self, path: &Path, source: PersistenceError) -> RegistryError {
        RegistryError::Storage {
            path: path.to_path_buf(),
            source,
        }
    }
}

/// Rejects an id that cannot name the file it is stored in.
///
/// Checked on the way in *and* on the way out (see [`entry_id`]): a directory
/// is a shared surface, and an entry may arrive in it without ever passing
/// through this crate.
fn validate_id(id: &str) -> Result<()> {
    if id.trim() != id || !is_usable_directory_name(id) {
        return Err(RegistryError::InvalidId { id: id.to_string() });
    }
    Ok(())
}

/// Whether a path is a registry entry by its extension.
fn has_entry_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| EXTENSIONS.contains(&extension))
}

/// The id a file name carries, when it carries a usable one.
fn entry_id(path: &Path) -> Option<String> {
    let stem = path.file_stem().and_then(|stem| stem.to_str())?;
    validate_id(stem).ok().map(|()| stem.to_string())
}
