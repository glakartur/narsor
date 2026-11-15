//! Writing a material's files into a local directory, safely.
//!
//! Two writers use it: the runner, delivering a material onto task storage,
//! and `narsor task materials download`, writing into the owner's chosen
//! folder. Both write below a root the *receiver* chose, from a manifest
//! someone else wrote, so the rules are the same:
//!
//! * every path is validated again ([`super::path::validate_path`]) — the
//!   receiver never trusts that the sender did;
//! * every blob is checked against its digest and size before anything is
//!   written;
//! * nothing below the root is followed through a symbolic link: an existing
//!   directory on the way must be a real directory, an existing file at the
//!   target a real file;
//! * existing files are refused unless overwriting was asked for, and all of
//!   that is checked for *every* file before the first is written, so a
//!   refusal leaves the directory as it was;
//! * each file is written to a temporary name beside its target and renamed
//!   over it — `rename(2)` replaces a link instead of writing through it.
//!
//! The root itself may be reached through links: choosing it is the
//! receiver's own decision.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use bytes::Bytes;

use super::{sha256_hex, validate_path, MaterialError, MaterialFile, MaterialResult};

/// What to do about a file that already exists at a target path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteMode {
    /// Refuse the whole write.
    Refuse,
    /// Replace it (a regular file only; a link or directory is refused).
    Overwrite,
}

/// Writes `files`, whose content is `blobs` by digest, below `root`, and
/// returns the paths written.
pub fn write_files(
    root: &Path,
    files: &[MaterialFile],
    blobs: &BTreeMap<String, Bytes>,
    mode: WriteMode,
) -> MaterialResult<Vec<PathBuf>> {
    let root_metadata = fs::metadata(root).map_err(|source| MaterialError::io(root, source))?;
    if !root_metadata.is_dir() {
        return Err(MaterialError::Refused(format!(
            "'{}' is not a directory",
            root.display()
        )));
    }
    for file in files {
        validate_path(&file.path)?;
        let blob = blobs
            .get(&file.sha256)
            .ok_or_else(|| MaterialError::Incomplete(format!("no content for '{}'", file.path)))?;
        if blob.len() as u64 != file.size || sha256_hex(blob) != file.sha256 {
            return Err(MaterialError::DigestMismatch(format!(
                "the content of '{}' does not match its manifest",
                file.path
            )));
        }
    }
    for file in files {
        check_target(root, &file.path, mode)?;
    }

    let mut written = Vec::with_capacity(files.len());
    for file in files {
        let target = root.join(&file.path);
        let mut dir = root.to_path_buf();
        let components: Vec<&str> = file.path.split('/').collect();
        for component in &components[..components.len() - 1] {
            dir.push(component);
            match fs::create_dir(&dir) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    require_real_dir(&dir)?
                }
                Err(source) => return Err(MaterialError::io(&dir, source)),
            }
        }
        write_one(&target, &blobs[&file.sha256])?;
        written.push(target);
    }
    Ok(written)
}

/// Refuses a target that would be reached through a link, or that exists
/// when it must not.
fn check_target(root: &Path, path: &str, mode: WriteMode) -> MaterialResult<()> {
    let components: Vec<&str> = path.split('/').collect();
    let mut current = root.to_path_buf();
    for component in &components[..components.len() - 1] {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(source) => return Err(MaterialError::io(&current, source)),
            Ok(_) => require_real_dir(&current)?,
        }
    }
    current.push(components[components.len() - 1]);
    match fs::symlink_metadata(&current) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(MaterialError::io(&current, source)),
        Ok(metadata) if metadata.file_type().is_symlink() => Err(MaterialError::Refused(format!(
            "'{}' is a symbolic link; refusing to write through it",
            current.display()
        ))),
        Ok(metadata) if !metadata.is_file() => Err(MaterialError::Refused(format!(
            "'{}' exists and is not a regular file",
            current.display()
        ))),
        Ok(_) if mode == WriteMode::Refuse => Err(MaterialError::Refused(format!(
            "'{}' already exists; refusing to overwrite it",
            current.display()
        ))),
        Ok(_) => Ok(()),
    }
}

fn require_real_dir(path: &Path) -> MaterialResult<()> {
    let metadata = fs::symlink_metadata(path).map_err(|source| MaterialError::io(path, source))?;
    if metadata.file_type().is_symlink() {
        return Err(MaterialError::Refused(format!(
            "'{}' is a symbolic link; refusing to write through it",
            path.display()
        )));
    }
    if !metadata.is_dir() {
        return Err(MaterialError::Refused(format!(
            "'{}' exists and is not a directory",
            path.display()
        )));
    }
    Ok(())
}

/// Writes `bytes` to a fresh temporary file beside `target`, then renames
/// it over `target`.
fn write_one(target: &Path, bytes: &[u8]) -> MaterialResult<()> {
    let name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temp = target.with_file_name(format!(
        ".{name}.{}.narsor-part",
        uuid::Uuid::new_v4().simple()
    ));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(|source| MaterialError::io(&temp, source))?;
        file.write_all(bytes)
            .map_err(|source| MaterialError::io(&temp, source))?;
        file.sync_all()
            .map_err(|source| MaterialError::io(&temp, source))?;
        fs::rename(&temp, target).map_err(|source| MaterialError::io(target, source))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}
