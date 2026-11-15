//! Advisory file locks shared by the file-backed stores.

use std::fs;
use std::os::fd::AsRawFd;
use std::path::PathBuf;

use super::PersistenceError;

/// Why a `FileLock` could not be taken.
#[derive(Debug, thiserror::Error)]
pub enum FileLockError {
    /// The lock file or its directory could not be prepared.
    #[error(transparent)]
    Persistence(#[from] PersistenceError),

    /// The advisory lock could not be taken.
    #[error("cannot lock '{path}': {source}")]
    Lock {
        /// The lock file.
        path: PathBuf,
        /// The underlying OS error.
        #[source]
        source: std::io::Error,
    },
}

/// An exclusive `flock(2)` held until dropped.
///
/// Advisory locks rather than create-new marker files: the kernel releases
/// them when the holder dies, so a crashed control plane can never strand a
/// task. Each acquisition opens its own file description, so the lock also
/// serializes threads of one process, not only processes.
#[derive(Debug)]
pub struct FileLock {
    file: fs::File,
}

impl FileLock {
    pub fn acquire(path: PathBuf) -> Result<Self, FileLockError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| PersistenceError::io(parent, source))?;
        }
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| PersistenceError::io(&path, source))?;
        loop {
            // SAFETY: `file` owns a valid descriptor for the duration of the call.
            let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
            if result == 0 {
                return Ok(Self { file });
            }
            let source = std::io::Error::last_os_error();
            if source.kind() != std::io::ErrorKind::Interrupted {
                return Err(FileLockError::Lock { path, source });
            }
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        // SAFETY: the descriptor is still owned by `self.file`. Closing it
        // would release the lock anyway; unlocking first just makes it prompt.
        let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}
