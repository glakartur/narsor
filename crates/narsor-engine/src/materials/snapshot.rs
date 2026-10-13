//! A one-off snapshot of local files as a material [`Bundle`] (R23).
//!
//! The owner's CLI snapshots the files and folders it is given; the runner
//! snapshots one directory of its task's outbox. Either way the snapshot is
//! a copy taken once — nothing keeps watching the source — and it holds
//! regular files only:
//!
//! * symbolic links are never followed, so a snapshot cannot reach outside
//!   the folder it was asked for;
//! * links, sockets, FIFOs and devices found inside a folder are skipped and
//!   reported ([`SkippedEntry`]), as are `.git` directories and names a
//!   material path cannot carry;
//! * a link or special file named *explicitly* is refused instead — the
//!   caller asked for exactly that and must not be left thinking it was
//!   sent.
//!
//! Size bounds are applied while walking, so an oversize folder is refused
//! before it is read in full.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use bytes::Bytes;

use super::{
    sha256_hex, validate_manifest, validate_path, Bundle, MaterialError, MaterialFile,
    MaterialLimits, MaterialResult, UploadManifest,
};

/// Something a snapshot left out, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedEntry {
    /// The local path.
    pub path: PathBuf,
    /// Why it was left out.
    pub reason: String,
}

/// A snapshot ready to upload.
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// The material.
    pub bundle: Bundle,
    /// What was left out.
    pub skipped: Vec<SkippedEntry>,
}

/// Snapshots `paths`: each file under its own name, each folder's content
/// under the folder's name.
pub fn snapshot_paths(paths: &[PathBuf], limits: &MaterialLimits) -> MaterialResult<Snapshot> {
    let mut walk = Walk::new(limits, limits.max_total_bytes);
    for path in paths {
        let metadata =
            fs::symlink_metadata(path).map_err(|source| MaterialError::io(path, source))?;
        let name = base_name(path)?;
        if metadata.file_type().is_symlink() {
            return Err(MaterialError::Refused(format!(
                "'{}' is a symbolic link; materials hold regular files only",
                path.display()
            )));
        }
        if metadata.is_dir() {
            walk.folder(path, &name)?;
        } else if metadata.is_file() {
            validate_path(&name)?;
            walk.file(path, name)?;
        } else {
            return Err(MaterialError::Refused(format!(
                "'{}' is not a regular file or folder",
                path.display()
            )));
        }
    }
    let label = match paths {
        [single] => base_name(single).ok(),
        _ => None,
    };
    walk.finish(label)
}

/// Snapshots the content of the folder `dir`, at the material's root.
pub fn snapshot_folder(
    dir: &Path,
    limits: &MaterialLimits,
    max_total: u64,
) -> MaterialResult<Snapshot> {
    let metadata = fs::symlink_metadata(dir).map_err(|source| MaterialError::io(dir, source))?;
    if !metadata.is_dir() {
        return Err(MaterialError::Refused(format!(
            "'{}' is not a folder",
            dir.display()
        )));
    }
    let mut walk = Walk::new(limits, max_total);
    walk.folder(dir, "")?;
    walk.finish(base_name(dir).ok())
}

fn base_name(path: &Path) -> MaterialResult<String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
        .ok_or_else(|| {
            MaterialError::Refused(format!(
                "'{}' has no usable file name (a UTF-8 name is required)",
                path.display()
            ))
        })
}

struct Walk<'a> {
    limits: &'a MaterialLimits,
    max_total: u64,
    total: u64,
    files: Vec<MaterialFile>,
    blobs: BTreeMap<String, Bytes>,
    skipped: Vec<SkippedEntry>,
}

impl<'a> Walk<'a> {
    fn new(limits: &'a MaterialLimits, max_total: u64) -> Self {
        Self {
            limits,
            max_total,
            total: 0,
            files: Vec::new(),
            blobs: BTreeMap::new(),
            skipped: Vec::new(),
        }
    }

    fn skip(&mut self, path: PathBuf, reason: impl Into<String>) {
        self.skipped.push(SkippedEntry {
            path,
            reason: reason.into(),
        });
    }

    /// Adds every regular file below `dir`, as `prefix/…`.
    fn folder(&mut self, dir: &Path, prefix: &str) -> MaterialResult<()> {
        let mut entries: Vec<_> = fs::read_dir(dir)
            .map_err(|source| MaterialError::io(dir, source))?
            .collect::<Result<_, _>>()
            .map_err(|source| MaterialError::io(dir, source))?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                self.skip(path, "its name is not UTF-8");
                continue;
            };
            let relative = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            // `DirEntry::file_type` does not follow links.
            let file_type = entry
                .file_type()
                .map_err(|source| MaterialError::io(&path, source))?;
            if file_type.is_symlink() {
                self.skip(path, "symbolic link (never followed)");
            } else if file_type.is_dir() {
                if name.eq_ignore_ascii_case(".git") {
                    self.skip(path, "Git metadata");
                } else {
                    self.folder(&path, &relative)?;
                }
            } else if file_type.is_file() {
                match validate_path(&relative) {
                    Ok(()) => self.file(&path, relative)?,
                    Err(error) => self.skip(path, error.to_string()),
                }
            } else {
                self.skip(path, "not a regular file (socket, FIFO or device)");
            }
        }
        Ok(())
    }

    /// Reads one regular file, refusing it from the moment it would break a
    /// bound.
    fn file(&mut self, path: &Path, relative: String) -> MaterialResult<()> {
        if self.files.len() >= self.limits.max_files {
            return Err(MaterialError::TooLarge(format!(
                "more than {} files",
                self.limits.max_files
            )));
        }
        let file = open_no_follow(path)?;
        let mut content = Vec::new();
        file.take(self.limits.max_file_bytes + 1)
            .read_to_end(&mut content)
            .map_err(|source| MaterialError::io(path, source))?;
        let size = content.len() as u64;
        if size > self.limits.max_file_bytes {
            return Err(MaterialError::TooLarge(format!(
                "'{}' is larger than {} bytes",
                path.display(),
                self.limits.max_file_bytes
            )));
        }
        self.total += size;
        if self.total > self.max_total {
            return Err(MaterialError::TooLarge(format!(
                "more than {} bytes in total",
                self.max_total
            )));
        }
        let sha256 = sha256_hex(&content);
        self.blobs
            .entry(sha256.clone())
            .or_insert_with(|| Bytes::from(content));
        self.files.push(MaterialFile {
            path: relative,
            size,
            sha256,
        });
        Ok(())
    }

    fn finish(self, label: Option<String>) -> MaterialResult<Snapshot> {
        let manifest = UploadManifest {
            material_id: None,
            label,
            ttl_secs: None,
            files: self.files,
        };
        validate_manifest(&manifest, self.limits, self.max_total)?;
        Ok(Snapshot {
            bundle: Bundle::new(manifest, self.blobs),
            skipped: self.skipped,
        })
    }
}

/// Opens a file for reading without following a link at its last component.
fn open_no_follow(path: &Path) -> MaterialResult<fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|source| MaterialError::io(path, source))
}
