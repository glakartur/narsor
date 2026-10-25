//! The environment catalogue on disk: backends, profiles, the runtime.
//!
//! ```text
//! {catalog}/backends/{id}.yaml   -- one execution backend per file
//! {catalog}/profiles/{id}.yaml   -- one user profile per file
//! {catalog}/runtime.yaml         -- runtime bundle and tool packages
//! ```
//!
//! The same layout and the same reasons as the repository registry
//! ([`crate::registry`]): one file per entry, the file name is the
//! identifier, entries are independent, and the directory maps onto a
//! ConfigMap one key at a time. Like the registry, nothing is cached: every
//! lookup reads the file, so a change made by the operator applies to the
//! *next* task created, and never to one that already resolved its
//! environment (R18).

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::Serialize;
use thiserror::Error;

use crate::persistence::{write_atomic, CatalogPaths, PersistenceError};

use super::backend::BackendRecord;
use super::profile::{ProfileRecord, RuntimeCatalog};

/// File extensions an entry may use, in lookup order.
const EXTENSIONS: [&str; 2] = ["yaml", "yml"];

/// A catalogue file that could not be used.
#[derive(Debug, Error)]
pub enum CatalogError {
    /// Reading or writing failed.
    #[error("environment catalogue '{path}' is unusable: {source}")]
    Storage {
        /// The file or directory.
        path: PathBuf,
        /// What failed.
        #[source]
        source: PersistenceError,
    },
    /// The file is there but is not a valid entry.
    #[error("environment catalogue entry '{path}' cannot be read: {message}")]
    Corrupt {
        /// The file.
        path: PathBuf,
        /// What is wrong.
        message: String,
    },
    /// An entry refused before it was stored.
    #[error("environment catalogue entry '{id}' is invalid: {}", problems.join("; "))]
    Invalid {
        /// The entry.
        id: String,
        /// Every problem found.
        problems: Vec<String>,
    },
}

/// Reads and writes the environment catalogue under one catalogue root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentCatalog {
    paths: CatalogPaths,
    builtin_local: bool,
}

impl EnvironmentCatalog {
    /// The catalogue under `root`. Nothing is read or created here.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            paths: CatalogPaths::new(root),
            builtin_local: true,
        }
    }

    /// The same catalogue without the built-in `local` backend: what a
    /// network server reads, which has no local controller (R13).
    pub fn without_builtin_local(mut self) -> Self {
        self.builtin_local = false;
        self
    }

    /// The backend files the catalogue declares, by id.
    pub fn declares_backend(&self, id: &str) -> Result<bool, CatalogError> {
        Ok(read_entry::<BackendRecord>(&self.paths.backends_dir(), id)?.is_some())
    }

    /// The backend record `id`, or `None` when none is registered.
    ///
    /// The daemon-owned `local` backend needs no declaration: when the
    /// catalogue defines none, a built-in record stands in — unless the
    /// catalogue was opened [`Self::without_builtin_local`]. The catalogue
    /// stays declaration-only, so it can be mounted read-only.
    pub fn backend(&self, id: &str) -> Result<Option<BackendRecord>, CatalogError> {
        let declared =
            read_entry::<BackendRecord>(&self.paths.backends_dir(), id)?.map(|mut record| {
                record.id = id.to_string();
                record
            });
        Ok(declared.or_else(|| {
            (self.builtin_local && id == BackendRecord::LOCAL_ID).then(BackendRecord::builtin_local)
        }))
    }

    /// Stores a backend record after validating it.
    pub fn save_backend(&self, record: &BackendRecord) -> Result<(), CatalogError> {
        refuse_problems(&record.id, record.problems())?;
        write_entry(
            &self
                .paths
                .backends_dir()
                .join(format!("{}.yaml", record.id)),
            record,
        )
    }

    /// The profile `id`, or `None` when none is defined.
    pub fn profile(&self, id: &str) -> Result<Option<ProfileRecord>, CatalogError> {
        Ok(
            read_entry::<ProfileRecord>(&self.paths.profiles_dir(), id)?.map(|mut record| {
                record.id = id.to_string();
                record
            }),
        )
    }

    /// Stores a profile after validating it.
    pub fn save_profile(&self, record: &ProfileRecord) -> Result<(), CatalogError> {
        refuse_problems(&record.id, record.problems())?;
        write_entry(
            &self
                .paths
                .profiles_dir()
                .join(format!("{}.yaml", record.id)),
            record,
        )
    }

    /// The runtime catalogue, or `None` when the file does not exist.
    pub fn runtime(&self) -> Result<Option<RuntimeCatalog>, CatalogError> {
        read_file(&self.paths.runtime_file())
    }

    /// Stores the runtime catalogue after validating it.
    pub fn save_runtime(&self, runtime: &RuntimeCatalog) -> Result<(), CatalogError> {
        refuse_problems("runtime", runtime.problems())?;
        write_entry(&self.paths.runtime_file(), runtime)
    }
}

fn refuse_problems(id: &str, problems: Vec<String>) -> Result<(), CatalogError> {
    if problems.is_empty() {
        Ok(())
    } else {
        Err(CatalogError::Invalid {
            id: id.to_string(),
            problems,
        })
    }
}

/// Reads `{dir}/{id}.yaml` (then `.yml`). An id that could not name a file
/// is simply absent: it can never have been stored.
fn read_entry<T: DeserializeOwned>(dir: &Path, id: &str) -> Result<Option<T>, CatalogError> {
    if !super::backend::is_reference_name(id) {
        return Ok(None);
    }
    for extension in EXTENSIONS {
        if let Some(entry) = read_file(&dir.join(format!("{id}.{extension}")))? {
            return Ok(Some(entry));
        }
    }
    Ok(None)
}

fn read_file<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, CatalogError> {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(source) if source.kind() == ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(CatalogError::Storage {
                path: path.to_path_buf(),
                source: PersistenceError::io(path, source),
            })
        }
    };
    serde_yaml::from_str(&contents)
        .map(Some)
        .map_err(|source| CatalogError::Corrupt {
            path: path.to_path_buf(),
            message: source.to_string(),
        })
}

fn write_entry<T: Serialize>(path: &Path, entry: &T) -> Result<(), CatalogError> {
    let text = serde_yaml::to_string(entry).map_err(|source| CatalogError::Corrupt {
        path: path.to_path_buf(),
        message: source.to_string(),
    })?;
    write_atomic(path, text.as_bytes()).map_err(|source| CatalogError::Storage {
        path: path.to_path_buf(),
        source,
    })
}
