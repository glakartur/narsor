//! The environment catalogue on disk: backends, profiles, the runtime.
//!
//! ```text
//! {catalog}/backends/{id}.yaml   -- one execution backend per file
//! {catalog}/profiles/{id}.yaml   -- one user profile per file
//! {catalog}/runtime/runtime.yaml -- runtime bundle, tool packages, task image
//! {catalog}/runtime/components/  -- the scripts the task image names
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

/// File extensions an entry (and a queue file of the automation catalogue)
/// may use, in lookup order.
pub(crate) const EXTENSIONS: [&str; 2] = ["yaml", "yml"];

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
    /// The runtime catalogue exists at its place and at the old place.
    #[error(
        "the runtime catalogue is defined twice, in '{current}' and in '{legacy}'; keep only \
         '{current}' (move what you need from the old file and its components/ directory into \
         the runtime directory, then delete '{legacy}')"
    )]
    RuntimeInTwoPlaces {
        /// `{catalog}/runtime/runtime.yaml` (or `.yml`).
        current: PathBuf,
        /// `{catalog}/runtime.yaml`.
        legacy: PathBuf,
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

    /// Whether the built-in `local` backend stands in for a missing
    /// declaration.
    pub fn has_builtin_local(&self) -> bool {
        self.builtin_local
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

    /// The runtime catalogue with the place it was read from, or `None`
    /// when there is none.
    ///
    /// It is `{catalog}/runtime/runtime.yaml` (then `.yml`). An older
    /// catalogue keeps it at `{catalog}/runtime.yaml`: that file is read
    /// while the new one does not exist, with one warning per process. Both
    /// at once is refused, so neither wins silently.
    pub fn runtime(&self) -> Result<Option<LocatedRuntime>, CatalogError> {
        let Some((file, legacy)) = self.locate_runtime()? else {
            return Ok(None);
        };
        let Some(catalog) = read_file::<RuntimeCatalog>(&file)? else {
            return Ok(None);
        };
        let script_dir = if legacy {
            warn_about_legacy_runtime(&file, &self.paths.runtime_file());
            self.paths.root().to_path_buf()
        } else {
            self.paths.runtime_dir()
        };
        Ok(Some(LocatedRuntime {
            catalog,
            file,
            script_dir,
            legacy,
        }))
    }

    /// Stores the runtime catalogue after validating it, into the file in
    /// use, else into `{catalog}/runtime/runtime.yaml`.
    pub fn save_runtime(&self, runtime: &RuntimeCatalog) -> Result<(), CatalogError> {
        refuse_problems("runtime", runtime.problems())?;
        let file = match self.locate_runtime()? {
            Some((file, _)) => file,
            None => self.paths.runtime_file(),
        };
        write_entry(&file, runtime)
    }

    /// The runtime catalogue file in use and whether it is at the old place.
    fn locate_runtime(&self) -> Result<Option<(PathBuf, bool)>, CatalogError> {
        let mut current = None;
        for extension in EXTENSIONS {
            let file = self
                .paths
                .runtime_dir()
                .join(format!("runtime.{extension}"));
            if file_exists(&file)? {
                current = Some(file);
                break;
            }
        }
        let legacy = self.paths.legacy_runtime_file();
        match (current, file_exists(&legacy)?) {
            (Some(current), true) => Err(CatalogError::RuntimeInTwoPlaces { current, legacy }),
            (Some(current), false) => Ok(Some((current, false))),
            (None, true) => Ok(Some((legacy, true))),
            (None, false) => Ok(None),
        }
    }
}

/// The runtime catalogue and where it was read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocatedRuntime {
    /// The catalogue itself.
    pub catalog: RuntimeCatalog,
    /// The file it was read from.
    pub file: PathBuf,
    /// The directory its script paths resolve from: the directory of
    /// `file`.
    pub script_dir: PathBuf,
    /// Whether `file` is at the old place, `{catalog}/runtime.yaml`.
    pub legacy: bool,
}

/// Logs, once per process, that the runtime catalogue is at the old place:
/// it is read on every task creation, so a warning per read would flood
/// the log.
fn warn_about_legacy_runtime(legacy: &Path, current: &Path) {
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| {
        let dir = current.parent().unwrap_or(current);
        tracing::warn!(
            old = %legacy.display(),
            new = %current.display(),
            "the runtime catalogue is at its old place '{}'; move it to '{}' and its \
             components/ directory into '{}' (the installer does this on update)",
            legacy.display(),
            current.display(),
            dir.display()
        );
    });
}

fn file_exists(path: &Path) -> Result<bool, CatalogError> {
    match std::fs::metadata(path) {
        Ok(_) => Ok(true),
        Err(source) if source.kind() == ErrorKind::NotFound => Ok(false),
        Err(source) => Err(CatalogError::Storage {
            path: path.to_path_buf(),
            source: PersistenceError::io(path, source),
        }),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A catalogue root in the temp directory, removed on drop.
    struct Root(PathBuf);

    impl Root {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("narsor-runtime-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&root).expect("catalogue root");
            Self(root)
        }

        fn write(&self, relative: &str, text: &str) {
            let path = self.0.join(relative);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
            std::fs::write(path, text).expect("write");
        }

        fn catalog(&self) -> EnvironmentCatalog {
            EnvironmentCatalog::new(&self.0)
        }
    }

    impl Drop for Root {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const RUNTIME: &str = "task_image:\n  base: debian:trixie-slim\n";

    fn empty_runtime() -> RuntimeCatalog {
        RuntimeCatalog {
            bundle: None,
            tools: Default::default(),
            task_image: None,
        }
    }

    #[test]
    fn the_runtime_catalogue_is_read_from_the_runtime_directory_or_the_old_place() {
        // (file present, directory its scripts resolve from, old place)
        let cases = [
            ("runtime/runtime.yaml", "runtime", false),
            ("runtime/runtime.yml", "runtime", false),
            ("runtime.yaml", "", true),
        ];
        for (file, script_dir, legacy) in cases {
            let root = Root::new();
            root.write(file, RUNTIME);

            let located = root
                .catalog()
                .runtime()
                .expect(file)
                .unwrap_or_else(|| panic!("{file} is found"));

            assert!(located.catalog.task_image.is_some(), "{file}");
            assert_eq!(located.file, root.0.join(file), "{file}");
            assert_eq!(located.script_dir, root.0.join(script_dir), "{file}");
            assert_eq!(located.legacy, legacy, "{file}");
        }
        assert!(Root::new().catalog().runtime().unwrap().is_none());
    }

    #[test]
    fn a_runtime_catalogue_in_both_places_is_refused_for_reads_and_writes() {
        let root = Root::new();
        root.write("runtime.yaml", RUNTIME);
        root.write("runtime/runtime.yaml", RUNTIME);
        let names_both = |error: CatalogError| {
            let text = error.to_string();
            assert!(
                text.contains(&root.0.join("runtime.yaml").display().to_string())
                    && text.contains(&root.0.join("runtime/runtime.yaml").display().to_string()),
                "{text}"
            );
        };

        names_both(root.catalog().runtime().expect_err("read"));
        names_both(
            root.catalog()
                .save_runtime(&empty_runtime())
                .expect_err("write"),
        );
    }

    #[test]
    fn a_runtime_write_goes_to_the_file_in_use_or_else_the_runtime_directory() {
        let fresh = Root::new();
        fresh.catalog().save_runtime(&empty_runtime()).unwrap();
        assert!(fresh.0.join("runtime/runtime.yaml").is_file());
        assert!(!fresh.0.join("runtime.yaml").exists());

        let old = Root::new();
        old.write("runtime.yaml", RUNTIME);
        old.catalog().save_runtime(&empty_runtime()).unwrap();
        let rewritten = std::fs::read_to_string(old.0.join("runtime.yaml")).unwrap();
        assert!(!rewritten.contains("debian"), "{rewritten}");
        assert!(!old.0.join("runtime").exists());
    }
}
