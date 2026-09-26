//! Atomic file writes: temp file, `fsync`, `rename`.
//!
//! A rename within a single directory is atomic on POSIX, so a concurrent
//! reader sees either the previous file or the new one, never a mixture, and a
//! process killed mid-write leaves the previous version complete on disk.
//!
//! The temporary file carries a random component
//! ([`unique_temp_path_for`](crate::persistence::paths::unique_temp_path_for))
//! rather than being derived from the target alone. Concurrent *writers* are
//! the reason: two of them sharing one temporary file interleave their bytes
//! into it, and the loser of the rename fails a write that should have
//! succeeded. Atomicity for the reader was never the part that was missing.

use super::{PersistenceError, Result};
use crate::persistence::paths::{is_temp_file, unique_temp_path_for};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Creates `dir` and all missing parents.
pub fn ensure_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).map_err(|source| PersistenceError::io(dir, source))
}

/// Writes `bytes` to `path` atomically, creating parent directories as needed.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        ensure_dir(parent)?;
    }

    let temp = unique_temp_path_for(path);
    write_and_sync(&temp, bytes)?;

    fs::rename(&temp, path).map_err(|source| {
        // Leave no orphan behind if even the rename failed.
        let _ = fs::remove_file(&temp);
        PersistenceError::io(path, source)
    })?;

    sync_parent_dir(path);
    Ok(())
}

/// Serializes `value` as pretty JSON and writes it to `path` atomically.
pub fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|source| PersistenceError::Serialize {
        path: path.to_path_buf(),
        source,
    })?;
    write_atomic(path, &bytes)
}

/// Writes `bytes` to `path` atomically, with the temporary file created under
/// `mode` from the moment it is opened — before a single byte is written to
/// it, not merely tightened afterwards.
///
/// [`write_atomic`] cannot be reused for secrets: it opens its temporary file
/// under the process umask and only the completed, renamed file would ever
/// get different permissions, which leaves a window where a temporary file
/// carrying the sensitive bytes exists world- or group-readable. Anything
/// written here (a daemon discovery file carrying a bearer token, say) needs
/// the restricted mode to apply to the very first byte on disk.
///
/// Unix-only, like the rest of this crate's permission handling
/// (`src/persistence/conversation.rs`, `src/workspace/provisioner.rs`); on
/// other platforms this falls back to [`write_atomic`] and ignores `mode`.
pub fn write_atomic_with_mode(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    if let Some(parent) = path.parent() {
        ensure_dir(parent)?;
    }

    let temp = unique_temp_path_for(path);
    write_and_sync_with_mode(&temp, bytes, mode)?;

    fs::rename(&temp, path).map_err(|source| {
        let _ = fs::remove_file(&temp);
        PersistenceError::io(path, source)
    })?;

    sync_parent_dir(path);
    Ok(())
}

/// Serializes `value` as pretty JSON and writes it to `path` atomically under
/// `mode`. See [`write_atomic_with_mode`].
pub fn write_json_atomic_with_mode<T: Serialize>(
    path: &Path,
    value: &T,
    mode: u32,
) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|source| PersistenceError::Serialize {
        path: path.to_path_buf(),
        source,
    })?;
    write_atomic_with_mode(path, &bytes, mode)
}

/// Writes `bytes` to `path` atomically, but fails if `path` already exists.
///
/// The content is written to a temporary file and then hard-linked into place:
/// `link(2)` fails with `EEXIST` rather than clobbering, which gives both
/// atomicity and `O_EXCL` semantics without a check-then-write race.
///
/// Returns `Ok(false)` when the target already existed, in which case the
/// existing file is left completely untouched — including when the caller
/// racing it is another process, because the temporary file is named uniquely
/// per write (see [`unique_temp_path_for`]).
pub fn write_atomic_exclusive(path: &Path, bytes: &[u8]) -> Result<bool> {
    if let Some(parent) = path.parent() {
        ensure_dir(parent)?;
    }

    let temp = unique_temp_path_for(path);
    write_and_sync(&temp, bytes)?;

    let linked = match fs::hard_link(&temp, path) {
        Ok(()) => true,
        Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(source) => {
            let _ = fs::remove_file(&temp);
            return Err(PersistenceError::io(path, source));
        }
    };

    fs::remove_file(&temp).map_err(|source| PersistenceError::io(&temp, source))?;
    if linked {
        sync_parent_dir(path);
    }
    Ok(linked)
}

/// Serializes `value` as pretty JSON and writes it only if `path` is absent.
pub fn write_json_atomic_exclusive<T: Serialize>(path: &Path, value: &T) -> Result<bool> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|source| PersistenceError::Serialize {
        path: path.to_path_buf(),
        source,
    })?;
    write_atomic_exclusive(path, &bytes)
}

/// Reads and deserializes a JSON file, naming the file in every error.
pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let bytes = fs::read(path).map_err(|source| PersistenceError::io(path, source))?;
    serde_json::from_slice(&bytes).map_err(|source| PersistenceError::Deserialize {
        path: path.to_path_buf(),
        source,
    })
}

/// Removes every `*.tmp` file directly inside `dir`.
///
/// A temporary file that outlived its writer is the trace of a crash between
/// the write and the rename: the target file still holds the last complete
/// version, so the leftover is garbage and never data to recover. A missing
/// directory is not an error — there is simply nothing to clean up.
///
/// Returns the paths that were removed.
pub fn cleanup_orphaned_temp_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(PersistenceError::io(dir, source)),
    };

    let mut removed = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| PersistenceError::io(dir, source))?;
        let path = entry.path();
        if !path.is_file() || !is_temp_file(&path) {
            continue;
        }
        fs::remove_file(&path).map_err(|source| PersistenceError::io(&path, source))?;
        tracing::warn!(path = %path.display(), "removed orphaned temporary file");
        removed.push(path);
    }
    Ok(removed)
}

/// Writes `bytes` to a fresh file and flushes it all the way to the device.
///
/// `pub(crate)` rather than private: [`crate::persistence::conversation`]
/// reuses it for the compression step, which is the same temp+fsync+rename
/// shape as every other atomic write in this module, just for bytes that
/// were not serialized from a `Serialize` value.
pub(crate) fn write_and_sync(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .map_err(|source| PersistenceError::io(path, source))?;
    file.write_all(bytes)
        .map_err(|source| PersistenceError::io(path, source))?;
    file.sync_all()
        .map_err(|source| PersistenceError::io(path, source))?;
    Ok(())
}

/// Writes `bytes` to a fresh file created under `mode`, flushed to the device.
///
/// Unix-only: opens the file with the mode already applied via
/// `OpenOptionsExt::mode`, so there is no window between file creation and
/// permission tightening. See [`write_atomic_with_mode`].
#[cfg(unix)]
pub(crate) fn write_and_sync_with_mode(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;

    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(path)
        .map_err(|source| PersistenceError::io(path, source))?;
    file.write_all(bytes)
        .map_err(|source| PersistenceError::io(path, source))?;
    file.sync_all()
        .map_err(|source| PersistenceError::io(path, source))?;
    Ok(())
}

/// Non-Unix fallback: `mode` is ignored, matching the process umask.
#[cfg(not(unix))]
pub(crate) fn write_and_sync_with_mode(path: &Path, bytes: &[u8], _mode: u32) -> Result<()> {
    write_and_sync(path, bytes)
}

/// Fsyncs the directory holding `path` so the rename itself is durable.
///
/// Best effort: some filesystems refuse to open a directory for sync, and a
/// failure here never invalidates the data that was already written.
///
/// `pub(crate)` for the same reason as [`write_and_sync`].
pub(crate) fn sync_parent_dir(path: &Path) {
    if let Some(parent) = path.parent() {
        if let Ok(dir) = File::open(parent) {
            let _ = dir.sync_all();
        }
    }
}
