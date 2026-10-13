//! Client-side material snapshots and safe downloads.
//!
//! The client may inspect files the operator selected and write material
//! responses below a destination they chose. It never opens the server's
//! material store.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use bytes::Bytes;
use narsor_protocol::api::{MaterialFile, UploadManifest};
use sha2::Digest as _;

const MAGIC: &[u8; 4] = b"NMB1";

#[derive(Debug, Clone)]
pub struct MaterialLimits {
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
    pub max_files: usize,
    pub max_manifest_bytes: usize,
}

impl Default for MaterialLimits {
    fn default() -> Self {
        Self {
            max_file_bytes: 16 * 1024 * 1024,
            max_total_bytes: 64 * 1024 * 1024,
            max_files: 1000,
            max_manifest_bytes: 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteMode {
    Refuse,
    Overwrite,
}

#[derive(Debug)]
pub struct MaterialError(String);

impl std::fmt::Display for MaterialError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for MaterialError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedEntry {
    pub path: PathBuf,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub bundle: Bundle,
    pub skipped: Vec<SkippedEntry>,
}

#[derive(Debug, Clone)]
pub struct Bundle {
    pub manifest: UploadManifest,
    blobs: BTreeMap<String, Bytes>,
    max_manifest_bytes: usize,
}

impl Bundle {
    pub fn encode(&self) -> Result<Vec<u8>, MaterialError> {
        let manifest = serde_json::to_vec(&self.manifest)
            .map_err(|error| MaterialError(format!("invalid material manifest: {error}")))?;
        if manifest.len() > self.max_manifest_bytes {
            return Err(MaterialError("the manifest is too large".to_string()));
        }
        let length = u32::try_from(manifest.len())
            .map_err(|_| MaterialError("the manifest is too large".to_string()))?;
        let mut unique = HashSet::new();
        let mut blob_bytes = 0usize;
        for file in &self.manifest.files {
            let blob = self
                .blobs
                .get(&file.sha256)
                .ok_or_else(|| MaterialError(format!("no content for blob {}", file.sha256)))?;
            if blob.len() as u64 != file.size {
                return Err(MaterialError(format!(
                    "the content of '{}' does not match its manifest",
                    file.path
                )));
            }
            if unique.insert(file.sha256.as_str()) {
                if sha256_hex(blob) != file.sha256 {
                    return Err(MaterialError(format!(
                        "the content of '{}' does not match its manifest",
                        file.path
                    )));
                }
                blob_bytes = blob_bytes
                    .checked_add(blob.len())
                    .ok_or_else(|| MaterialError("material size overflow".into()))?;
            }
        }
        let capacity = 8usize
            .checked_add(manifest.len())
            .and_then(|size| size.checked_add(blob_bytes))
            .ok_or_else(|| MaterialError("material size overflow".into()))?;
        let mut body = Vec::with_capacity(capacity);
        body.extend_from_slice(MAGIC);
        body.extend_from_slice(&length.to_be_bytes());
        body.extend_from_slice(&manifest);
        let mut sent = HashSet::new();
        for file in &self.manifest.files {
            if sent.insert(file.sha256.as_str()) {
                let blob = self
                    .blobs
                    .get(&file.sha256)
                    .ok_or_else(|| MaterialError(format!("no content for blob {}", file.sha256)))?;
                body.extend_from_slice(blob);
            }
        }
        Ok(body)
    }
}

pub fn snapshot_paths(
    paths: &[PathBuf],
    limits: &MaterialLimits,
) -> Result<Snapshot, MaterialError> {
    let mut walk = Walker {
        limits,
        total: 0,
        files: Vec::new(),
        blobs: BTreeMap::new(),
        skipped: Vec::new(),
    };
    for path in paths {
        let metadata = fs::symlink_metadata(path).map_err(|error| io_error(path, error))?;
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                MaterialError(format!(
                    "'{}' has no usable UTF-8 file name",
                    path.display()
                ))
            })?;
        if metadata.file_type().is_symlink() {
            return Err(MaterialError(format!(
                "'{}' is a symbolic link; materials hold regular files only",
                path.display()
            )));
        }
        if metadata.is_dir() {
            walk.folder(path, name)?;
        } else if metadata.is_file() {
            validate_path(name)?;
            walk.file(path, name.to_string())?;
        } else {
            return Err(MaterialError(format!(
                "'{}' is not a regular file or folder",
                path.display()
            )));
        }
    }
    let label = match paths {
        [single] => single
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_owned),
        _ => None,
    };
    let manifest = UploadManifest {
        material_id: None,
        label,
        ttl_secs: None,
        files: walk.files,
    };
    Ok(Snapshot {
        bundle: Bundle {
            manifest,
            blobs: walk.blobs,
            max_manifest_bytes: limits.max_manifest_bytes,
        },
        skipped: walk.skipped,
    })
}

struct Walker<'a> {
    limits: &'a MaterialLimits,
    total: u64,
    files: Vec<MaterialFile>,
    blobs: BTreeMap<String, Bytes>,
    skipped: Vec<SkippedEntry>,
}

impl Walker<'_> {
    fn skip(&mut self, path: PathBuf, reason: &str) {
        self.skipped.push(SkippedEntry {
            path,
            reason: reason.to_string(),
        });
    }

    fn folder(&mut self, dir: &Path, prefix: &str) -> Result<(), MaterialError> {
        let mut entries = fs::read_dir(dir)
            .map_err(|error| io_error(dir, error))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| io_error(dir, error))?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                self.skip(path, "its name is not UTF-8");
                continue;
            };
            let relative = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            let kind = entry.file_type().map_err(|error| io_error(&path, error))?;
            if kind.is_symlink() {
                self.skip(path, "symbolic link (never followed)");
            } else if kind.is_dir() {
                if name.eq_ignore_ascii_case(".git") {
                    self.skip(path, "Git metadata");
                } else {
                    self.folder(&path, &relative)?;
                }
            } else if kind.is_file() {
                if let Err(error) = validate_path(&relative) {
                    self.skip(path, &error.to_string());
                } else {
                    self.file(&path, relative)?;
                }
            } else {
                self.skip(path, "not a regular file or folder");
            }
        }
        Ok(())
    }

    fn file(&mut self, path: &Path, relative: String) -> Result<(), MaterialError> {
        let metadata = fs::symlink_metadata(path).map_err(|error| io_error(path, error))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(MaterialError(format!(
                "'{}' is not a regular file",
                path.display()
            )));
        }
        if metadata.len() > self.limits.max_file_bytes {
            return Err(MaterialError(format!(
                "'{}' exceeds the per-file material size limit",
                path.display()
            )));
        }
        if self.files.len() >= self.limits.max_files {
            return Err(MaterialError("the material has too many files".to_string()));
        }
        self.total = self
            .total
            .checked_add(metadata.len())
            .ok_or_else(|| MaterialError("material size overflow".into()))?;
        if self.total > self.limits.max_total_bytes {
            return Err(MaterialError(
                "the material exceeds the total size limit".to_string(),
            ));
        }
        let file = open_no_follow(path)?;
        let opened_metadata = file.metadata().map_err(|error| io_error(path, error))?;
        if !opened_metadata.is_file() {
            return Err(MaterialError(format!(
                "'{}' is not a regular file",
                path.display()
            )));
        }
        let capacity = usize::try_from(metadata.len())
            .map_err(|_| MaterialError("material size exceeds platform limits".into()))?;
        let mut bytes = Vec::with_capacity(capacity);
        file.take(self.limits.max_file_bytes.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|error| io_error(path, error))?;
        if bytes.len() as u64 > self.limits.max_file_bytes {
            return Err(MaterialError(format!(
                "'{}' exceeds the per-file material size limit",
                path.display()
            )));
        }
        if bytes.len() as u64 != metadata.len() {
            return Err(MaterialError(format!(
                "'{}' changed while it was being read",
                path.display()
            )));
        }
        let sha256 = sha256_hex(&bytes);
        let size = bytes.len() as u64;
        self.blobs
            .entry(sha256.clone())
            .or_insert_with(|| Bytes::from(bytes));
        self.files.push(MaterialFile {
            path: relative,
            size,
            sha256,
        });
        Ok(())
    }
}

pub fn write_files(
    root: &Path,
    files: &[MaterialFile],
    blobs: &BTreeMap<String, Bytes>,
    mode: WriteMode,
) -> Result<Vec<PathBuf>, MaterialError> {
    if !fs::metadata(root)
        .map_err(|error| io_error(root, error))?
        .is_dir()
    {
        return Err(MaterialError(format!(
            "'{}' is not a directory",
            root.display()
        )));
    }
    let mut verified = HashSet::new();
    for file in files {
        validate_path(&file.path)?;
        let blob = blobs
            .get(&file.sha256)
            .ok_or_else(|| MaterialError(format!("no content for '{}'", file.path)))?;
        if blob.len() as u64 != file.size
            || (verified.insert(file.sha256.as_str()) && sha256_hex(blob) != file.sha256)
        {
            return Err(MaterialError(format!(
                "the content of '{}' does not match its manifest",
                file.path
            )));
        }
        check_target(root, &file.path, mode)?;
    }
    let mut written = Vec::with_capacity(files.len());
    for file in files {
        let target = root.join(&file.path);
        let mut directory = root.to_path_buf();
        let components: Vec<_> = file.path.split('/').collect();
        for component in &components[..components.len() - 1] {
            directory.push(component);
            match fs::create_dir(&directory) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    require_real_dir(&directory)?
                }
                Err(error) => return Err(io_error(&directory, error)),
            }
        }
        let temp = target.with_file_name(format!(
            ".{}.{}.narsor-part",
            target.file_name().unwrap_or_default().to_string_lossy(),
            uuid::Uuid::new_v4().simple()
        ));
        let result = (|| {
            let mut output = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)
                .map_err(|error| io_error(&temp, error))?;
            output
                .write_all(&blobs[&file.sha256])
                .map_err(|error| io_error(&temp, error))?;
            output.sync_all().map_err(|error| io_error(&temp, error))?;
            fs::rename(&temp, &target).map_err(|error| io_error(&target, error))
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result?;
        written.push(target);
    }
    Ok(written)
}

fn check_target(root: &Path, path: &str, mode: WriteMode) -> Result<(), MaterialError> {
    let components: Vec<_> = path.split('/').collect();
    let mut current = root.to_path_buf();
    for component in &components[..components.len() - 1] {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(io_error(&current, error)),
            Ok(_) => require_real_dir(&current)?,
        }
    }
    current.push(components[components.len() - 1]);
    match fs::symlink_metadata(&current) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error(&current, error)),
        Ok(metadata) if metadata.file_type().is_symlink() => Err(MaterialError(format!(
            "'{}' is a symbolic link; refusing to write through it",
            current.display()
        ))),
        Ok(metadata) if !metadata.is_file() => Err(MaterialError(format!(
            "'{}' is not a regular file",
            current.display()
        ))),
        Ok(_) if mode == WriteMode::Refuse => Err(MaterialError(format!(
            "'{}' already exists; refusing to overwrite it",
            current.display()
        ))),
        Ok(_) => Ok(()),
    }
}

fn require_real_dir(path: &Path) -> Result<(), MaterialError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| io_error(path, error))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(MaterialError(format!(
            "'{}' is not a real directory",
            path.display()
        )));
    }
    Ok(())
}

fn validate_path(path: &str) -> Result<(), MaterialError> {
    if path.is_empty() || path.len() > 1024 || path.starts_with('/') || path.contains('\\') {
        return Err(MaterialError(format!(
            "'{path}' is not a safe relative path"
        )));
    }
    let components: Vec<_> = path.split('/').collect();
    if components.len() > 32
        || components.iter().any(|part| {
            part.is_empty()
                || part.len() > 255
                || *part == "."
                || *part == ".."
                || part.contains(':')
                || part.eq_ignore_ascii_case(".git")
                || part.chars().any(char::is_control)
        })
    {
        return Err(MaterialError(format!(
            "'{path}' is not a safe relative path"
        )));
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = sha2::Sha256::digest(bytes);
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

fn open_no_follow(path: &Path) -> Result<fs::File, MaterialError> {
    use std::os::unix::fs::OpenOptionsExt as _;
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| io_error(path, error))
}

fn io_error(path: &Path, error: std::io::Error) -> MaterialError {
    MaterialError(format!("I/O error on '{}': {error}", path.display()))
}
