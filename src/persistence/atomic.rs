//! Atomic file writes: temp file, `fsync`, `rename`.
//!
//! A rename within a single directory is atomic on POSIX, so a concurrent
//! reader sees either the previous file or the new one, never a mixture, and a
//! process killed mid-write leaves the previous version complete on disk.

use super::{PersistenceError, Result};
use crate::persistence::paths::{is_temp_file, temp_path_for};
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

    let temp = temp_path_for(path);
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

/// Writes `bytes` to `path` atomically, but fails if `path` already exists.
///
/// The content is written to a temporary file and then hard-linked into place:
/// `link(2)` fails with `EEXIST` rather than clobbering, which gives both
/// atomicity and `O_EXCL` semantics without a check-then-write race.
///
/// Returns `Ok(false)` when the target already existed, in which case the
/// existing file is left completely untouched.
pub fn write_atomic_exclusive(path: &Path, bytes: &[u8]) -> Result<bool> {
    if let Some(parent) = path.parent() {
        ensure_dir(parent)?;
    }

    let temp = temp_path_for(path);
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
fn write_and_sync(path: &Path, bytes: &[u8]) -> Result<()> {
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

/// Fsyncs the directory holding `path` so the rename itself is durable.
///
/// Best effort: some filesystems refuse to open a directory for sync, and a
/// failure here never invalidates the data that was already written.
fn sync_parent_dir(path: &Path) {
    if let Some(parent) = path.parent() {
        if let Ok(dir) = File::open(parent) {
            let _ = dir.sync_all();
        }
    }
}
