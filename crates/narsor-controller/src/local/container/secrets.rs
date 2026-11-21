//! The token store of a local controller (KTD10): the secrets the operator
//! stored once with `narsor-cli secret set <name>`, kept for the containers
//! that need them.
//!
//! The store is the `secrets` directory beside the controller credential
//! (mode 0700), with one file per name (mode 0600). A value is written to a
//! temporary file and renamed over the old one, so a reader never sees half
//! a value. For each container start, [`SecretStore::copy_into`] copies the
//! entries a task needs into a per-workload directory that is mounted
//! read-only and handed to the runner as `--secrets-dir`.
//!
//! Names follow the secret reference rule of the catalogue
//! ([`is_reference_name`]), so a name can never leave the directory. No
//! function here logs or prints a value.

use std::fs;
use std::io::{self, Write as _};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use narsor_model::domain::is_reference_name;

/// The name of the store directory beside the controller credential.
pub const STORE_DIR: &str = "secrets";

/// Mode of the store directory and of a per-workload directory.
const DIR_MODE: u32 = 0o700;

/// Mode of every secret file.
const FILE_MODE: u32 = 0o600;

/// Why the token store refused or failed an operation. No variant carries a
/// value.
#[derive(Debug)]
pub enum SecretError {
    /// The name does not follow the secret reference rule.
    InvalidName(String),
    /// The value is empty.
    EmptyValue(String),
    /// The store has no entry for these names.
    Missing(Vec<String>),
    /// A file operation failed.
    Io { path: PathBuf, source: io::Error },
}

impl std::fmt::Display for SecretError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SecretError::InvalidName(name) => write!(
                f,
                "'{name}' is not a secret name: use letters, digits, '-', '_' and '.', not \
                 starting with '.'"
            ),
            SecretError::EmptyValue(name) => {
                write!(
                    f,
                    "the value of secret '{name}' is empty; nothing was stored"
                )
            }
            SecretError::Missing(names) => {
                let commands: Vec<String> = names
                    .iter()
                    .map(|name| format!("`narsor-cli secret set {name}`"))
                    .collect();
                write!(
                    f,
                    "the token store has no secret {}; store it with {}",
                    names
                        .iter()
                        .map(|name| format!("'{name}'"))
                        .collect::<Vec<_>>()
                        .join(", "),
                    commands.join(", ")
                )
            }
            SecretError::Io { path, source } => {
                write!(f, "cannot use {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for SecretError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SecretError::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// The token store of one controller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretStore {
    dir: PathBuf,
}

impl SecretStore {
    /// The store of the controller keeping `credential_file`: the
    /// [`STORE_DIR`] directory beside it.
    pub fn beside_credential(credential_file: &Path) -> Self {
        let parent = credential_file
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        Self {
            dir: parent.join(STORE_DIR),
        }
    }

    /// The store directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Stores `value` under `name`, replacing an earlier value.
    pub fn set(&self, name: &str, value: &str) -> Result<(), SecretError> {
        check_name(name)?;
        if value.is_empty() {
            return Err(SecretError::EmptyValue(name.to_string()));
        }
        create_private_dir(&self.dir)?;
        write_atomically(&self.dir, name, value.as_bytes())
    }

    /// The value stored under `name`, or `None` when there is none.
    pub fn read(&self, name: &str) -> Result<Option<String>, SecretError> {
        check_name(name)?;
        let path = self.dir.join(name);
        match fs::read_to_string(&path) {
            Ok(value) => Ok(Some(value)),
            Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(SecretError::Io { path, source }),
        }
    }

    /// Copies the entries `names` into `dir` (created with mode 0700, each
    /// file 0600), for a workload's `--secrets-dir`. Every name must be in
    /// the store: when some are not, nothing is copied and the error names
    /// all of them.
    pub fn copy_into<'a>(
        &self,
        names: impl IntoIterator<Item = &'a str>,
        dir: &Path,
    ) -> Result<(), SecretError> {
        let mut values = Vec::new();
        let mut missing = Vec::new();
        for name in names {
            match self.read(name)? {
                Some(value) => values.push((name, value)),
                None => missing.push(name.to_string()),
            }
        }
        if !missing.is_empty() {
            return Err(SecretError::Missing(missing));
        }
        create_private_dir(dir)?;
        for (name, value) in values {
            write_atomically(dir, name, value.as_bytes())?;
        }
        Ok(())
    }
}

/// The value of a secret as it came on stdin: without one trailing newline
/// (`\n` or `\r\n`), everything else kept.
pub fn value_from_input(input: &str) -> &str {
    input
        .strip_suffix('\n')
        .map(|rest| rest.strip_suffix('\r').unwrap_or(rest))
        .unwrap_or(input)
}

fn check_name(name: &str) -> Result<(), SecretError> {
    if is_reference_name(name) {
        Ok(())
    } else {
        Err(SecretError::InvalidName(name.to_string()))
    }
}

/// Creates `dir` (and its missing parents) with mode 0700, and narrows an
/// existing one to it.
fn create_private_dir(dir: &Path) -> Result<(), SecretError> {
    let io_error = |source| SecretError::Io {
        path: dir.to_path_buf(),
        source,
    };
    fs::DirBuilder::new()
        .recursive(true)
        .mode(DIR_MODE)
        .create(dir)
        .map_err(io_error)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(DIR_MODE)).map_err(io_error)
}

/// Writes `contents` to `dir/name` (mode 0600) through a temporary file
/// renamed over it. A valid name never starts with '.', so the temporary
/// name cannot be an entry.
fn write_atomically(dir: &Path, name: &str, contents: &[u8]) -> Result<(), SecretError> {
    let target = dir.join(name);
    let temporary = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let written = (|| {
        let _ = fs::remove_file(&temporary);
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(FILE_MODE)
            .open(&temporary)?;
        file.write_all(contents)?;
        file.sync_all()?;
        fs::rename(&temporary, &target)
    })();
    written.map_err(|source| {
        let _ = fs::remove_file(&temporary);
        SecretError::Io {
            path: target,
            source,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A temporary directory removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "narsor-secrets-{label}-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4()
            ));
            fs::create_dir_all(&dir).expect("create the directory");
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).expect("metadata").permissions().mode() & 0o777
    }

    fn store(temp: &TempDir) -> SecretStore {
        SecretStore::beside_credential(&temp.0.join("controller").join("credential"))
    }

    #[test]
    fn a_stored_value_reads_back_from_private_files_beside_the_credential() {
        let temp = TempDir::new("set");
        let store = store(&temp);

        store.set("gitlab-token", "first").unwrap();
        store.set("gitlab-token", "glpat-secret").unwrap();

        assert_eq!(store.dir(), temp.0.join("controller").join(STORE_DIR));
        assert_eq!(
            store.read("gitlab-token").unwrap().as_deref(),
            Some("glpat-secret")
        );
        assert_eq!(store.read("other").unwrap(), None);
        assert_eq!(mode(store.dir()), 0o700);
        assert_eq!(mode(&store.dir().join("gitlab-token")), 0o600);
        let entries: Vec<_> = fs::read_dir(store.dir()).unwrap().collect();
        assert_eq!(entries.len(), 1, "no temporary file is left behind");
    }

    #[test]
    fn invalid_names_and_empty_values_are_refused() {
        let temp = TempDir::new("refused");
        let store = store(&temp);
        let cases = [
            ("../x", "value", "InvalidName"),
            ("", "value", "InvalidName"),
            (".hidden", "value", "InvalidName"),
            ("a/b", "value", "InvalidName"),
            ("token", "", "EmptyValue"),
        ];
        for (name, value, expected) in cases {
            let error = store.set(name, value).unwrap_err();
            let kind = match error {
                SecretError::InvalidName(_) => "InvalidName",
                SecretError::EmptyValue(_) => "EmptyValue",
                _ => "other",
            };
            assert_eq!(kind, expected, "set({name:?}, {value:?})");
        }
        assert!(matches!(
            store.read("../x"),
            Err(SecretError::InvalidName(_))
        ));
        assert!(!temp.0.join("x").exists());
    }

    #[test]
    fn copying_into_a_workload_directory_names_every_missing_secret() {
        let temp = TempDir::new("copy");
        let store = store(&temp);
        store.set("gitlab-token", "glpat-secret").unwrap();
        let workload = temp.0.join("workload").join("secrets");

        let error = store
            .copy_into(["gitlab-token", "github-token", "npm-token"], &workload)
            .unwrap_err();
        let message = error.to_string();
        assert!(matches!(error, SecretError::Missing(ref names) if names.len() == 2));
        assert!(
            message.contains("narsor-cli secret set github-token"),
            "{message}"
        );
        assert!(
            message.contains("narsor-cli secret set npm-token"),
            "{message}"
        );
        assert!(
            !workload.exists(),
            "nothing is copied when a secret is missing"
        );

        store.copy_into(["gitlab-token"], &workload).unwrap();
        assert_eq!(
            fs::read_to_string(workload.join("gitlab-token")).unwrap(),
            "glpat-secret"
        );
        assert_eq!(mode(&workload), 0o700);
        assert_eq!(mode(&workload.join("gitlab-token")), 0o600);
    }

    #[test]
    fn input_loses_one_trailing_newline_only() {
        let cases = [
            ("token\n", "token"),
            ("token\r\n", "token"),
            ("token", "token"),
            ("token\n\n", "token\n"),
            (" token \n", " token "),
            ("\n", ""),
        ];
        for (input, expected) in cases {
            assert_eq!(value_from_input(input), expected, "{input:?}");
        }
    }
}
