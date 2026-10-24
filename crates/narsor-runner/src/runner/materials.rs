//! The runner's half of task materials (U9).
//!
//! **Delivery.** A poll answer announces owner materials the control plane
//! has not seen acknowledged. For each, the runner fetches the manifest and
//! every blob with its own incarnation-bound credential — the only channel
//! it has, never a mount of anybody's machine (R23) — checks every digest,
//! writes the files into `narsor/materials.partial/{id}` and renames that to
//! `materials/{id}`. The rename is the commit: a `materials/{id}` directory
//! is complete by construction, so a redelivery after a lost acknowledgement
//! only acknowledges again, and a delivery cut short leaves nothing but a
//! partial directory the next attempt starts over.
//!
//! **Publication.** Each folder the task leaves in `outbox/` is published as
//! a material once no engine pass runs (nothing is still writing it), with
//! an id derived from its name and content so a retry after a lost answer
//! is recognized. Published folders are removed; a folder the control plane
//! refuses (too large, a name a material cannot carry) is renamed to
//! `<name>.rejected` and left for the owner to inspect.
//!
//! Neither directory is inside the workspace, so none of this ever touches
//! the product repository (R30, AE7).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::api::ClientError;
use crate::controller_protocol::RunnerClient;
use crate::materials::{
    blob_order, is_material_id, manifest_digest, sha256_hex, snapshot_folder, write_files, Bundle,
    MaterialAnnouncement, MaterialError, MaterialFile, MaterialLimits, MaterialView, WriteMode,
};

use super::storage::TaskStorage;
use super::Session;

/// Why one delivery did not complete.
#[derive(Debug)]
enum DeliveryError {
    /// The control plane did not answer as needed.
    Client(ClientError),
    /// The material or the storage was refused.
    Material(MaterialError),
}

impl std::fmt::Display for DeliveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Client(error) => error.fmt(f),
            Self::Material(error) => error.fmt(f),
        }
    }
}

impl From<ClientError> for DeliveryError {
    fn from(error: ClientError) -> Self {
        Self::Client(error)
    }
}

impl From<MaterialError> for DeliveryError {
    fn from(error: MaterialError) -> Self {
        Self::Material(error)
    }
}

impl Session {
    /// Whether materials are being backed off after a failed delivery.
    pub(super) fn materials_deferred(&self) -> bool {
        self.material_retry_at
            .is_some_and(|at| tokio::time::Instant::now() < at)
    }

    /// Writes the announced materials onto task storage and acknowledges
    /// each.
    ///
    /// A material that cannot be delivered right now — the control plane
    /// unreachable, a blob that does not match its manifest, a full disk —
    /// is retried after a growing delay, during which the poll defers
    /// materials, instead of in a tight loop of immediate poll answers.
    pub(super) async fn receive_materials(&mut self, announcements: Vec<MaterialAnnouncement>) {
        let mut failed = false;
        for announcement in announcements {
            let Some(client) = self.link.client() else {
                return;
            };
            let outcome = match deliver(&self.storage, &client, &announcement).await {
                Ok(()) => client
                    .ack_material(&announcement.material_id, &announcement.manifest_digest)
                    .await
                    .map(|_| ())
                    .map_err(DeliveryError::Client),
                Err(error) => Err(error),
            };
            match outcome {
                Ok(()) => tracing::info!(
                    material = %announcement.material_id,
                    files = announcement.file_count,
                    "material delivered onto task storage"
                ),
                Err(error) => {
                    failed = true;
                    tracing::warn!(material = %announcement.material_id, %error, "material not delivered");
                    if let DeliveryError::Client(error @ ClientError::Unauthorized) = &error {
                        self.link.failed(error, &self.timing);
                    }
                }
            }
        }
        if failed {
            self.material_failures = self.material_failures.saturating_add(1);
            let factor = 2u32.saturating_pow(self.material_failures.saturating_sub(1).min(16));
            let delay = self
                .timing
                .retry_min
                .saturating_mul(factor)
                .min(self.timing.retry_max);
            self.material_retry_at = Some(tokio::time::Instant::now() + delay);
        } else {
            self.material_failures = 0;
            self.material_retry_at = None;
        }
    }

    /// Publishes every finished folder of the outbox.
    pub(super) async fn publish_outbox(&mut self) {
        self.outbox_due = false;
        let entries = match outbox_entries(&self.storage.outbox_dir()) {
            Ok(entries) => entries,
            Err(error) => {
                tracing::warn!(%error, "cannot read the outbox");
                return;
            }
        };
        for (name, dir) in entries {
            let Some(client) = self.link.client() else {
                self.outbox_due = true;
                return;
            };
            let bundle = match outbox_bundle(&name, &dir) {
                Ok(bundle) => bundle,
                Err(error) => {
                    reject(&dir, &error.to_string());
                    continue;
                }
            };
            let body = match bundle.encode() {
                Ok(body) => body,
                Err(error) => {
                    reject(&dir, &error.to_string());
                    continue;
                }
            };
            match client.publish_material(body).await {
                Ok(response) => {
                    tracing::info!(
                        material = %response.material.material_id,
                        outbox = %name,
                        "published an outbox folder as a material"
                    );
                    if let Err(error) = fs::remove_dir_all(&dir) {
                        tracing::warn!(dir = %dir.display(), %error, "cannot clear a published outbox folder");
                    }
                }
                Err(ClientError::Request { message, .. }) => reject(&dir, &message),
                Err(error) => {
                    self.link.failed(&error, &self.timing);
                    self.outbox_due = true;
                    return;
                }
            }
        }
    }
}

/// Fetches, verifies and writes one material; nothing to do when it is on
/// storage already.
async fn deliver(
    storage: &TaskStorage,
    client: &RunnerClient,
    announcement: &MaterialAnnouncement,
) -> Result<(), DeliveryError> {
    if delivered_before(storage, announcement)? {
        // Written by an earlier attempt whose acknowledgement was lost.
        return Ok(());
    }
    let material_id = &announcement.material_id;
    let view = client.material(material_id).await?;
    check_manifest(announcement, &view)?;
    let mut blobs = BTreeMap::new();
    for (sha256, size) in blob_order(&view.files) {
        let bytes = client.material_blob(material_id, &sha256).await?;
        check_blob(material_id, &sha256, size, &bytes)?;
        blobs.insert(sha256, bytes);
    }
    write_material(storage, material_id, &view.files, &blobs)?;
    Ok(())
}

/// Whether the announced material is on storage already, complete; an id
/// a material cannot have is refused.
fn delivered_before(
    storage: &TaskStorage,
    announcement: &MaterialAnnouncement,
) -> Result<bool, MaterialError> {
    let material_id = &announcement.material_id;
    if !is_material_id(material_id) {
        return Err(MaterialError::InvalidManifest(format!(
            "material id '{material_id}'"
        )));
    }
    let target = storage.materials_dir().join(material_id);
    Ok(fs::symlink_metadata(&target).is_ok_and(|metadata| metadata.is_dir()))
}

/// Refuses a manifest that is not the one announced.
fn check_manifest(
    announcement: &MaterialAnnouncement,
    view: &MaterialView,
) -> Result<(), MaterialError> {
    if view.summary.manifest_digest != announcement.manifest_digest
        || manifest_digest(&view.files) != announcement.manifest_digest
    {
        return Err(MaterialError::DigestMismatch(format!(
            "the manifest of '{}' is not the one announced",
            announcement.material_id
        )));
    }
    Ok(())
}

/// Refuses blob bytes that do not match their manifest entry.
fn check_blob(
    material_id: &str,
    sha256: &str,
    size: u64,
    bytes: &[u8],
) -> Result<(), MaterialError> {
    if bytes.len() as u64 != size || sha256_hex(bytes) != sha256 {
        return Err(MaterialError::DigestMismatch(format!(
            "blob {sha256} of '{material_id}' does not match its manifest"
        )));
    }
    Ok(())
}

/// Writes checked files into the partial directory, then renames it to
/// `materials/{id}`: the rename is the commit.
fn write_material(
    storage: &TaskStorage,
    material_id: &str,
    files: &[MaterialFile],
    blobs: &BTreeMap<String, bytes::Bytes>,
) -> Result<(), MaterialError> {
    let partial = storage.partial_materials_dir().join(material_id);
    remove_if_present(&partial)?;
    fs::create_dir_all(&partial).map_err(|source| MaterialError::io(&partial, source))?;
    write_files(&partial, files, blobs, WriteMode::Refuse)?;
    let materials = storage.materials_dir();
    fs::create_dir_all(&materials).map_err(|source| MaterialError::io(&materials, source))?;
    let target = materials.join(material_id);
    fs::rename(&partial, &target).map_err(|source| MaterialError::io(&target, source))?;
    Ok(())
}

fn remove_if_present(dir: &Path) -> Result<(), MaterialError> {
    match fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(MaterialError::io(dir, source)),
    }
}

/// Folders of the outbox waiting to be published, by name: hidden folders
/// and rejected ones are left alone, and anything that is not a real folder
/// is ignored.
fn outbox_entries(outbox: &Path) -> std::io::Result<Vec<(String, PathBuf)>> {
    let entries = match fs::read_dir(outbox) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut folders = Vec::new();
    for entry in entries {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if name.starts_with('.') || name.ends_with(REJECTED_SUFFIX) {
            continue;
        }
        if entry.file_type()?.is_dir() {
            folders.push((name, entry.path()));
        }
    }
    folders.sort();
    Ok(folders)
}

const REJECTED_SUFFIX: &str = ".rejected";

/// The material an outbox folder is published as: its files, labelled with
/// the folder's name, under an id from its name and content.
fn outbox_bundle(name: &str, dir: &Path) -> Result<Bundle, MaterialError> {
    let limits = MaterialLimits::default();
    let snapshot = snapshot_folder(dir, &limits, limits.max_published_bytes)?;
    for skipped in &snapshot.skipped {
        tracing::warn!(path = %skipped.path.display(), reason = %skipped.reason, "outbox entry not published");
    }
    let mut bundle = snapshot.bundle;
    let digest = manifest_digest(&bundle.manifest.files);
    bundle.manifest.material_id = Some(outbox_material_id(name, &digest));
    bundle.manifest.label = Some(name.to_string());
    Ok(bundle)
}

/// The id of an outbox folder's material: the same name and content always
/// give the same id, so a publication repeated after a lost answer is
/// recognized by the control plane instead of stored twice.
fn outbox_material_id(name: &str, digest: &str) -> String {
    format!(
        "out-{}",
        &sha256_hex(format!("{name}\n{digest}").as_bytes())[..24]
    )
}

/// Sets a refused outbox folder aside, so it is not offered again.
fn reject(dir: &Path, reason: &str) {
    tracing::warn!(dir = %dir.display(), reason, "outbox folder refused; renamed to *.rejected");
    let name = dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut target = dir.with_file_name(format!("{name}{REJECTED_SUFFIX}"));
    if target.exists() {
        target = dir.with_file_name(format!(
            "{name}-{}{REJECTED_SUFFIX}",
            uuid::Uuid::new_v4().simple()
        ));
    }
    if let Err(error) = fs::rename(dir, &target) {
        tracing::warn!(dir = %dir.display(), %error, "cannot set a refused outbox folder aside");
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use chrono::Utc;

    use super::*;
    use crate::materials::{MaterialOrigin, MaterialSummary};

    /// A task storage that deletes itself.
    struct Storage(TaskStorage);

    impl Storage {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("narsor-materials-{}", uuid::Uuid::new_v4()));
            let storage = TaskStorage::new(root);
            storage.prepare().unwrap();
            Self(storage)
        }
    }

    impl Drop for Storage {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(self.0.root());
        }
    }

    /// The manifest entries and blobs of `files` (path, content).
    fn material(files: &[(&str, &[u8])]) -> (Vec<MaterialFile>, BTreeMap<String, Bytes>) {
        let mut blobs = BTreeMap::new();
        let entries = files
            .iter()
            .map(|(path, content)| {
                let sha256 = sha256_hex(content);
                blobs.insert(sha256.clone(), Bytes::copy_from_slice(content));
                MaterialFile {
                    path: path.to_string(),
                    size: content.len() as u64,
                    sha256,
                }
            })
            .collect();
        (entries, blobs)
    }

    fn announcement(material_id: &str, files: &[MaterialFile]) -> MaterialAnnouncement {
        MaterialAnnouncement {
            material_id: material_id.to_string(),
            manifest_digest: manifest_digest(files),
            file_count: files.len() as u32,
            total_bytes: files.iter().map(|file| file.size).sum(),
        }
    }

    fn view(material_id: &str, files: &[MaterialFile], digest: &str) -> MaterialView {
        MaterialView {
            summary: MaterialSummary {
                material_id: material_id.to_string(),
                task_id: "task-1".to_string(),
                origin: MaterialOrigin::Owner,
                label: None,
                created_at: Utc::now(),
                expires_at: Utc::now(),
                file_count: files.len() as u32,
                total_bytes: files.iter().map(|file| file.size).sum(),
                manifest_digest: digest.to_string(),
                delivered_at: None,
            },
            files: files.to_vec(),
        }
    }

    /// Relative path to content of every file below `dir`.
    fn tree(dir: &Path) -> BTreeMap<String, Vec<u8>> {
        let mut files = BTreeMap::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(current) = stack.pop() {
            for entry in fs::read_dir(&current).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    let relative = path.strip_prefix(dir).unwrap().display().to_string();
                    files.insert(relative, fs::read(&path).unwrap());
                }
            }
        }
        files
    }

    /// A checked material lands whole under its id, exactly as its
    /// manifest says, outside the workspace (R30, AE7).
    #[test]
    fn a_checked_material_is_written_whole_under_its_id() {
        let storage = Storage::new();
        let (files, blobs) = material(&[
            ("brief/docs/notes.md", b"secret content"),
            ("brief/plan.txt", b"step one\n"),
        ]);
        let announced = announcement("brief-1", &files);
        assert!(!delivered_before(&storage.0, &announced).unwrap());
        check_manifest(
            &announced,
            &view("brief-1", &files, &announced.manifest_digest),
        )
        .unwrap();
        for file in &files {
            check_blob("brief-1", &file.sha256, file.size, &blobs[&file.sha256]).unwrap();
        }

        write_material(&storage.0, "brief-1", &files, &blobs).unwrap();

        let written = tree(&storage.0.materials_dir().join("brief-1"));
        let expected: BTreeMap<String, Vec<u8>> = [
            (
                "brief/docs/notes.md".to_string(),
                b"secret content".to_vec(),
            ),
            ("brief/plan.txt".to_string(), b"step one\n".to_vec()),
        ]
        .into_iter()
        .collect();
        assert_eq!(written, expected, "exactly the manifest, nothing more");
        assert!(!storage.0.partial_materials_dir().join("brief-1").exists());
        assert!(!storage
            .0
            .materials_dir()
            .starts_with(storage.0.workspace_dir()));
        assert!(delivered_before(&storage.0, &announced).unwrap());
    }

    /// A material an earlier incarnation wrote (its acknowledgement was
    /// lost) is acknowledged again, not fetched or rewritten.
    #[test]
    fn a_material_on_storage_already_is_not_written_again() {
        let storage = Storage::new();
        let (files, _) = material(&[("a.txt", b"alpha")]);
        let earlier = storage.0.materials_dir().join("kept");
        fs::create_dir_all(&earlier).unwrap();
        fs::write(earlier.join("written-by-the-earlier-incarnation"), "").unwrap();

        assert!(delivered_before(&storage.0, &announcement("kept", &files)).unwrap());
        assert!(earlier.join("written-by-the-earlier-incarnation").exists());
        assert!(
            delivered_before(&storage.0, &announcement("../escape", &files)).is_err(),
            "an id a material cannot have is refused"
        );
    }

    /// Content that does not match its manifest, or a manifest that is not
    /// the one announced, is refused before anything is written.
    #[test]
    fn a_tampered_material_is_refused_before_anything_is_written() {
        let storage = Storage::new();
        let (files, _) = material(&[("b.txt", b"bravo")]);
        let announced = announcement("tampered", &files);

        let blob = check_blob("tampered", &files[0].sha256, files[0].size, b"BRAVO");
        assert!(
            matches!(blob, Err(MaterialError::DigestMismatch(_))),
            "{blob:?}"
        );
        let (other, _) = material(&[("b.txt", b"other")]);
        let manifest = check_manifest(
            &announced,
            &view("tampered", &other, &announced.manifest_digest),
        );
        assert!(
            matches!(manifest, Err(MaterialError::DigestMismatch(_))),
            "{manifest:?}"
        );

        assert!(!storage.0.materials_dir().join("tampered").exists());
        assert!(!storage.0.partial_materials_dir().join("tampered").exists());
    }

    /// A folder the task leaves in its outbox is published under its name,
    /// with an id from its name and content; hidden and refused folders are
    /// left alone.
    #[test]
    fn an_outbox_folder_is_published_under_its_name_with_a_stable_id() {
        let storage = Storage::new();
        let outbox = storage.0.outbox_dir();
        assert!(!outbox.starts_with(storage.0.workspace_dir()));
        fs::create_dir_all(outbox.join("report")).unwrap();
        fs::write(outbox.join("report/notes.md"), "temporary documentation").unwrap();
        fs::create_dir_all(outbox.join(".hidden")).unwrap();
        fs::create_dir_all(outbox.join("old.rejected")).unwrap();
        fs::write(outbox.join("loose-file"), "not a folder").unwrap();

        let entries = outbox_entries(&outbox).unwrap();
        assert_eq!(entries, vec![("report".to_string(), outbox.join("report"))]);
        let bundle = outbox_bundle("report", &outbox.join("report")).unwrap();
        assert_eq!(bundle.manifest.label.as_deref(), Some("report"));
        let files: Vec<&str> = bundle
            .manifest
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect();
        assert_eq!(files, vec!["notes.md"]);
        let id = bundle.manifest.material_id.clone().unwrap();
        assert!(is_material_id(&id), "{id}");
        assert_eq!(
            outbox_bundle("report", &outbox.join("report"))
                .unwrap()
                .manifest
                .material_id,
            Some(id.clone()),
            "a retry is recognized by its id"
        );
        fs::write(outbox.join("report/notes.md"), "changed").unwrap();
        assert_ne!(
            outbox_bundle("report", &outbox.join("report"))
                .unwrap()
                .manifest
                .material_id,
            Some(id)
        );
    }

    /// A folder the control plane refused is set aside, not offered again.
    #[test]
    fn a_refused_outbox_folder_is_set_aside() {
        let storage = Storage::new();
        let outbox = storage.0.outbox_dir();
        fs::create_dir_all(outbox.join("report")).unwrap();

        reject(&outbox.join("report"), "too large");

        assert!(!outbox.join("report").exists());
        assert!(outbox.join("report.rejected").is_dir());
        assert!(outbox_entries(&outbox).unwrap().is_empty());
    }
}
