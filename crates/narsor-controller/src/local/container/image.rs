//! The task image of the container work mode (KTD12): built from the sealed
//! [`TaskImage`] definition in a background job.
//!
//! [`TaskImageBuilds`] keeps at most one build job per definition hash (the
//! sha256 of the sealed definition). A caller asks with
//! [`TaskImageBuilds::request`] when a workload starts and polls with
//! [`TaskImageBuilds::poll`]; both return at once, so the reconcile step
//! never waits for a build.
//!
//! Each hash has a cache entry, `<controller dir>/images/<hash>.json`, with
//! the last `version` outputs of the components, the time of that check,
//! the base image id and the last image id. A request starts a job when:
//! - there is no image for the hash (or it is gone from Docker);
//! - the check is older than the update interval (`Some(0)` checks at every
//!   request, `None` never checks again after the first build);
//! - the refresh marker `<controller dir>/images/refresh` (written by
//!   `narsor-cli image refresh`) is newer than the check.
//!
//! The job runs every `version` script on the host, pulls the base and
//! builds one layer per component. The `version` output is the build
//! argument of the component's layer, so a new version rebuilds that layer
//! and the ones after it. When neither a version nor the base changed, the
//! previous image is kept without a build.
//!
//! A failed job is either permanent (an install script or the definition
//! failed: the definition reports [`BuildState::Failed`] until a refresh or
//! the next due check) or retryable (the daemon did not answer, a call timed
//! out, the registry could not be reached: the definition reports
//! [`BuildState::Retrying`] and the job runs again after
//! [`BuildSettings::retry_delay`]).
//!
//! [`TaskImageBuilds::collect_garbage`] removes the images this controller
//! built (the [`CONTROLLER_LABEL`] of its directory) that are neither named
//! by the caller nor the latest image of a definition still in use: one an
//! image the caller names was built from, or one requested within the update
//! interval (at least a day; a week without an interval). A definition out
//! of use loses its cache entry too. Images of other controllers on the same
//! daemon are never touched. It does not prune the build cache: Docker
//! cannot filter the build cache by label, so pruning it would remove the
//! operator's own build cache too.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Read as _};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{mpsc, Arc, Mutex, MutexGuard, PoisonError, RwLock, TryLockError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use narsor_model::domain::{TaskImage, TaskImageComponent};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::docker::{BuildRequest, DockerError, DockerTransport, UserIds};

/// The directory of the cache entries and the refresh marker, beside the
/// controller credential.
pub const IMAGES_DIR: &str = "images";

/// The file whose modification time asks for a re-check of every
/// definition.
pub const REFRESH_MARKER: &str = "refresh";

/// The label every narsor-built image carries.
pub const MANAGED_LABEL: &str = "io.narsor.managed";
/// The label with the definition hash of the image.
pub const DEFINITION_LABEL: &str = "io.narsor.definition";
/// The label with the build time (RFC 3339).
pub const BUILT_AT_LABEL: &str = "io.narsor.built-at";
/// The label of the controller that built the image: the first 16 hex
/// digits of the sha256 of its canonical directory.
pub const CONTROLLER_LABEL: &str = "io.narsor.controller";

/// How long after a retryable failure the job runs again.
pub const RETRY_DELAY: Duration = Duration::from_secs(60);

/// How long a definition nobody requests keeps its image without an update
/// interval.
const UNUSED_DEFINITION_KEEP: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// The shortest time a definition nobody requests keeps its image, however
/// short the update interval.
const MIN_UNUSED_DEFINITION_KEEP: Duration = Duration::from_secs(24 * 60 * 60);

/// The suffix of the file whose modification time is the last request of a
/// definition.
const REQUESTED_SUFFIX: &str = ".requested";

/// The repository of the content tags.
const TAG_REPOSITORY: &str = "narsor-task-image";

/// Where the component scripts are in the image.
const SCRIPTS_IN_IMAGE: &str = "/opt/narsor/components";

/// The user and group name of the added passwd and group entries.
const IMAGE_USER: &str = "narsor";

/// The environment variables a `version` script gets from the controller's
/// own environment; it gets nothing else.
const VERSION_SCRIPT_ENV: [&str; 3] = ["PATH", "HOME", "LANG"];

/// What a caller learns about the image of a definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildState {
    /// A job checks versions or builds; poll again later.
    Building,
    /// The last job failed for a reason that may pass (the daemon or the
    /// registry did not answer): why. A job runs again soon; poll again
    /// later.
    Retrying(String),
    /// The image to run, by id (`sha256:…`).
    Ready(String),
    /// No image exists and the last job failed: why.
    Failed(String),
}

/// How the build jobs run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildSettings {
    /// How long a check of the versions stays valid (see the module doc).
    pub update_interval: Option<Duration>,
    /// How long a `version` script may run.
    pub version_timeout: Duration,
    /// How many bytes of a `version` script's output count.
    pub version_output_cap: usize,
    /// The host user the passwd and group entries are for.
    pub user: UserIds,
    /// How long after a retryable failure the job runs again.
    pub retry_delay: Duration,
}

impl BuildSettings {
    /// The production settings: the current process user, a 60 s timeout
    /// and a 1 KiB output cap.
    pub fn new(update_interval: Option<Duration>) -> Self {
        // SAFETY: `getuid` and `getgid` take nothing and cannot fail.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        Self {
            update_interval,
            version_timeout: Duration::from_secs(60),
            version_output_cap: 1024,
            user: UserIds { uid, gid },
            retry_delay: RETRY_DELAY,
        }
    }
}

/// The registry of the image build jobs of one controller.
#[derive(Debug, Clone)]
pub struct TaskImageBuilds {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    docker: Arc<dyn DockerTransport>,
    /// `<controller dir>/images`.
    dir: PathBuf,
    /// The value of [`CONTROLLER_LABEL`] on the images this controller
    /// builds.
    controller_id: String,
    settings: BuildSettings,
    /// What this process knows per definition hash. Never held across a
    /// Docker call.
    slots: Mutex<HashMap<String, Slot>>,
    /// Held shared by every running job, exclusively by garbage
    /// collection: a collection never sees a job's image before it is
    /// cached, and is skipped while a job runs.
    gate: RwLock<()>,
}

#[derive(Debug, Clone)]
enum Slot {
    Running,
    /// The last job, started at `started`, failed and no image exists. A
    /// retryable failure runs again at `retry_at`.
    Failed {
        message: String,
        started: SystemTime,
        retry_at: Option<SystemTime>,
    },
}

/// The cache entry of one definition hash.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct CacheEntry {
    /// The last `version` output per component name.
    #[serde(default)]
    versions: BTreeMap<String, String>,
    /// When the versions were last checked, in ms since the epoch.
    checked_at_ms: u64,
    /// The id of the base image the last image was built on.
    #[serde(default)]
    base_id: Option<String>,
    /// The latest image of the definition.
    #[serde(default)]
    image_id: Option<String>,
}

/// The sha256 (hex) of the sealed definition: the key of its job, its cache
/// entry and its images.
pub fn definition_hash(image: &TaskImage) -> String {
    let canonical = serde_json::to_vec(image).expect("a task image serializes");
    hex(&Sha256::digest(canonical))
}

/// The refresh marker of the controller whose directory is `controller_dir`.
pub fn refresh_marker(controller_dir: &Path) -> PathBuf {
    controller_dir.join(IMAGES_DIR).join(REFRESH_MARKER)
}

/// Asks for a re-check of every definition at its next request: writes the
/// refresh marker, whose modification time is the request time.
pub fn request_refresh(controller_dir: &Path) -> io::Result<PathBuf> {
    let marker = refresh_marker(controller_dir);
    create_private_dir(marker.parent().expect("the marker has a directory"))?;
    fs::write(&marker, b"")?;
    Ok(marker)
}

impl TaskImageBuilds {
    /// The registry of the controller whose directory (the parent of its
    /// credential) is `controller_dir`.
    pub fn new(
        docker: Arc<dyn DockerTransport>,
        controller_dir: &Path,
        settings: BuildSettings,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                docker,
                dir: controller_dir.join(IMAGES_DIR),
                controller_id: controller_id(controller_dir),
                settings,
                slots: Mutex::new(HashMap::new()),
                gate: RwLock::new(()),
            }),
        }
    }

    /// The image for `image` when a workload starts. Starts a job when the
    /// cached image is missing, out of date or a refresh was asked for;
    /// returns at once.
    pub fn request(&self, image: &TaskImage) -> BuildState {
        self.decide(image, true)
    }

    /// The image for `image` while a workload waits for it. Never starts a
    /// re-check of a known image or outcome; starts a job only when this
    /// controller knows nothing about the definition (after a restart
    /// during a build).
    pub fn poll(&self, image: &TaskImage) -> BuildState {
        self.decide(image, false)
    }

    /// Removes the images this controller built that are not in
    /// `referenced` (the images per-task and live workload records name)
    /// and not the latest image of a definition still in use (see the
    /// module doc); forgets the definitions out of use. Does nothing while a
    /// job runs: its new image is not cached yet. Returns how many images
    /// were removed.
    pub fn collect_garbage(&self, referenced: &HashSet<String>) -> Result<usize, DockerError> {
        let inner = &self.inner;
        let _gate = match inner.gate.try_write() {
            Ok(gate) => gate,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => return Ok(0),
        };
        let mut used_definitions = HashSet::new();
        for id in referenced {
            if let Some(info) = inner.docker.inspect_image(id)? {
                used_definitions.extend(info.labels.get(DEFINITION_LABEL).cloned());
            }
        }
        let keep_unused = inner.unused_keep();
        let now = SystemTime::now();
        let mut keep: HashSet<String> = referenced.clone();
        for (hash, entry) in inner.cache_entries() {
            let last_used = inner
                .requested_time(&hash)
                .map_or(from_ms(entry.checked_at_ms), |requested| {
                    requested.max(from_ms(entry.checked_at_ms))
                });
            // A time in the future (a clock stepped back) counts as now.
            let recent = now
                .duration_since(last_used)
                .map_or(true, |age| age < keep_unused);
            if used_definitions.contains(&hash) || recent {
                keep.extend(entry.image_id);
            } else {
                inner.forget(&hash);
            }
        }
        let filters = [
            format!("{MANAGED_LABEL}=true"),
            format!("{CONTROLLER_LABEL}={}", inner.controller_id),
        ];
        let mut removed = 0;
        for id in inner.docker.list_images(&filters)? {
            if keep.contains(&id) {
                continue;
            }
            match inner.docker.remove_image(&id) {
                Ok(()) => removed += 1,
                // An image a container still uses cannot go; try next time.
                Err(error) => tracing::warn!("task image: cannot remove {id}: {error}"),
            }
        }
        Ok(removed)
    }

    fn decide(&self, image: &TaskImage, recheck: bool) -> BuildState {
        let inner = &self.inner;
        let hash = definition_hash(image);
        if recheck {
            inner.note_request(&hash);
        }
        let cache = inner.read_cache(&hash);
        let marker = inner.marker_time();
        if let Some(state) = inner.slot_state(&hash, cache.image_id.is_some(), recheck, marker) {
            return state;
        }
        if let Some(id) = &cache.image_id {
            if !recheck {
                return BuildState::Ready(id.clone());
            }
            // Docker is asked without the lock: a slow answer holds up no
            // other definition.
            let exists = matches!(inner.docker.inspect_image(id), Ok(Some(_)));
            if exists && !inner.due(from_ms(cache.checked_at_ms), marker) {
                return BuildState::Ready(id.clone());
            }
        }
        let mut slots = inner.lock_slots();
        if matches!(slots.get(&hash), Some(Slot::Running)) {
            // Another request started the job meanwhile.
            return BuildState::Building;
        }
        slots.insert(hash.clone(), Slot::Running);
        drop(slots);
        let job = Job {
            inner: Arc::clone(inner),
            hash,
            image: image.clone(),
            started: SystemTime::now(),
        };
        std::thread::Builder::new()
            .name("task-image-build".into())
            .spawn(move || job.run())
            .expect("a build job thread starts");
        BuildState::Building
    }
}

impl Inner {
    fn lock_slots(&self) -> MutexGuard<'_, HashMap<String, Slot>> {
        self.slots.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// What the slot of `hash` answers on its own: `None` lets the cache
    /// and Docker decide.
    fn slot_state(
        &self,
        hash: &str,
        has_image: bool,
        recheck: bool,
        marker: Option<SystemTime>,
    ) -> Option<BuildState> {
        let slots = self.lock_slots();
        match slots.get(hash)? {
            Slot::Running => Some(BuildState::Building),
            Slot::Failed { .. } if has_image => None,
            Slot::Failed {
                message,
                started,
                retry_at: Some(retry_at),
            } => {
                let refreshed = marker.is_some_and(|marker| marker > *started);
                (!refreshed && SystemTime::now() < *retry_at)
                    .then(|| BuildState::Retrying(message.clone()))
            }
            Slot::Failed {
                message,
                started,
                retry_at: None,
            } => (!recheck || !self.due(*started, marker))
                .then(|| BuildState::Failed(message.clone())),
        }
    }

    fn cache_path(&self, hash: &str) -> PathBuf {
        self.dir.join(format!("{hash}.json"))
    }

    fn requested_path(&self, hash: &str) -> PathBuf {
        self.dir.join(format!("{hash}{REQUESTED_SUFFIX}"))
    }

    /// Records that a workload start asked for `hash` now.
    fn note_request(&self, hash: &str) {
        let written =
            create_private_dir(&self.dir).and_then(|()| fs::write(self.requested_path(hash), b""));
        if let Err(error) = written {
            tracing::warn!(
                "task image {}: cannot record the request: {error}",
                short(hash)
            );
        }
    }

    fn requested_time(&self, hash: &str) -> Option<SystemTime> {
        fs::metadata(self.requested_path(hash))
            .and_then(|metadata| metadata.modified())
            .ok()
    }

    /// Forgets a definition out of use: its cache entry and request time.
    fn forget(&self, hash: &str) {
        let _ = fs::remove_file(self.cache_path(hash));
        let _ = fs::remove_file(self.requested_path(hash));
    }

    /// How long a definition nobody requests keeps its image.
    fn unused_keep(&self) -> Duration {
        self.settings
            .update_interval
            .map_or(UNUSED_DEFINITION_KEEP, |interval| {
                interval.max(MIN_UNUSED_DEFINITION_KEEP)
            })
    }

    /// The cache entry of `hash`; an unreadable entry counts as none.
    fn read_cache(&self, hash: &str) -> CacheEntry {
        let path = self.cache_path(hash);
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
                tracing::warn!("task image: ignoring {}: {error}", path.display());
                CacheEntry::default()
            }),
            Err(_) => CacheEntry::default(),
        }
    }

    fn write_cache(&self, hash: &str, entry: &CacheEntry) -> io::Result<()> {
        create_private_dir(&self.dir)?;
        let path = self.cache_path(hash);
        let temp = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(entry).map_err(io::Error::other)?;
        fs::write(&temp, bytes)?;
        fs::rename(&temp, &path)
    }

    /// Every cache entry, by definition hash.
    fn cache_entries(&self) -> Vec<(String, CacheEntry)> {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let name = entry.file_name().into_string().ok()?;
                let hash = name.strip_suffix(".json")?.to_string();
                let cache = self.read_cache(&hash);
                Some((hash, cache))
            })
            .collect()
    }

    fn marker_time(&self) -> Option<SystemTime> {
        fs::metadata(self.dir.join(REFRESH_MARKER))
            .and_then(|metadata| metadata.modified())
            .ok()
    }

    /// Whether a check made at `checked_at` must be made again.
    fn due(&self, checked_at: SystemTime, marker: Option<SystemTime>) -> bool {
        if marker.is_some_and(|marker| marker > checked_at) {
            return true;
        }
        match self.settings.update_interval {
            None => false,
            Some(interval) => SystemTime::now()
                .duration_since(checked_at)
                .map_or(true, |age| age >= interval),
        }
    }
}

/// Why a job ended without an image.
#[derive(Debug, Clone, PartialEq, Eq)]
struct JobFailure {
    message: String,
    /// Whether the cause may pass on its own (the daemon or the registry
    /// did not answer), so the job runs again soon.
    retryable: bool,
}

impl JobFailure {
    fn permanent(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: false,
        }
    }
}

/// One background job: check, pull, build, record.
struct Job {
    inner: Arc<Inner>,
    hash: String,
    image: TaskImage,
    /// When the job was started: a refresh asked for after it runs the
    /// definition again even when the job fails.
    started: SystemTime,
}

impl Job {
    fn run(self) {
        let outcome = {
            let _gate = self
                .inner
                .gate
                .read()
                .unwrap_or_else(PoisonError::into_inner);
            self.check_and_build()
        };
        let mut slots = self.inner.lock_slots();
        match outcome {
            Ok(()) => {
                slots.remove(&self.hash);
            }
            Err(JobFailure { message, retryable }) => {
                tracing::warn!("task image {}: {message}", short(&self.hash));
                let delay = self.inner.settings.retry_delay;
                let (message, retry_at) = if retryable {
                    let message = format!("{message} (trying again in {}s)", delay.as_secs());
                    (message, Some(SystemTime::now() + delay))
                } else {
                    let message = format!(
                        "{message} (after a fix outside the definition, `narsor-cli image \
                         refresh` tries again)"
                    );
                    (message, None)
                };
                slots.insert(
                    self.hash.clone(),
                    Slot::Failed {
                        message,
                        started: self.started,
                        retry_at,
                    },
                );
            }
        }
    }

    /// Ends with a cached image, or fails when no image exists for the
    /// definition.
    fn check_and_build(&self) -> Result<(), JobFailure> {
        let inner = &self.inner;
        let docker = &inner.docker;
        let previous = inner.read_cache(&self.hash);
        let checked_at = SystemTime::now();
        let versions = self.check_versions(&previous.versions);

        let base = &self.image.base.reference;
        if let Err(error) = docker.pull_image(base) {
            tracing::warn!("task image: cannot pull the base {base}, using a local copy: {error}");
        }
        let base_id = docker
            .inspect_image(base)
            .ok()
            .flatten()
            .map(|info| info.id);
        let previous_image = previous
            .image_id
            .clone()
            .filter(|id| matches!(docker.inspect_image(id), Ok(Some(_))));

        let unchanged = versions == previous.versions && base_id == previous.base_id;
        if let (true, Some(id)) = (unchanged, &previous_image) {
            return self.record(&CacheEntry {
                checked_at_ms: to_ms(checked_at),
                image_id: Some(id.clone()),
                ..previous
            });
        }

        match self.build(&versions, base_id.as_deref()) {
            Ok(id) => self.record(&CacheEntry {
                versions,
                checked_at_ms: to_ms(checked_at),
                base_id,
                image_id: Some(id),
            }),
            Err(failure) => match previous_image {
                Some(id) => {
                    tracing::warn!(
                        "task image {}: {}; keeping the previous image {id}",
                        short(&self.hash),
                        failure.message
                    );
                    // The old versions stay, so the next check builds again.
                    self.record(&CacheEntry {
                        checked_at_ms: to_ms(checked_at),
                        image_id: Some(id),
                        ..previous
                    })
                }
                None => Err(failure),
            },
        }
    }

    fn record(&self, entry: &CacheEntry) -> Result<(), JobFailure> {
        self.inner.write_cache(&self.hash, entry).map_err(|error| {
            JobFailure::permanent(format!("cannot write the image cache: {error}"))
        })
    }

    /// The `version` output of every component that has a script. A script
    /// that fails keeps its previous output.
    fn check_versions(&self, previous: &BTreeMap<String, String>) -> BTreeMap<String, String> {
        let settings = &self.inner.settings;
        let mut versions = BTreeMap::new();
        for component in &self.image.components {
            let Some(script) = &component.version else {
                continue;
            };
            match run_version_script(
                script,
                settings.version_timeout,
                settings.version_output_cap,
            ) {
                Ok(output) => {
                    versions.insert(component.name.clone(), output);
                }
                Err(error) => {
                    tracing::warn!(
                        "task image: the version script of component '{}' {error}; keeping its \
                         previous version",
                        component.name
                    );
                    if let Some(output) = previous.get(&component.name) {
                        versions.insert(component.name.clone(), output.clone());
                    }
                }
            }
        }
        versions
    }

    /// Builds the image in a fresh context; returns its id.
    fn build(
        &self,
        versions: &BTreeMap<String, String>,
        base_id: Option<&str>,
    ) -> Result<String, JobFailure> {
        let context = self.inner.dir.join(format!(
            "build-{}-{}",
            short(&self.hash),
            uuid::Uuid::new_v4().simple()
        ));
        let result = self.build_in(&context, versions, base_id);
        let _ = fs::remove_dir_all(&context);
        result
    }

    fn build_in(
        &self,
        context: &Path,
        versions: &BTreeMap<String, String>,
        base_id: Option<&str>,
    ) -> Result<String, JobFailure> {
        check_definition(&self.image).map_err(JobFailure::permanent)?;
        let settings = &self.inner.settings;
        write_context(context, &self.image, settings.user).map_err(|error| {
            JobFailure::permanent(format!("cannot write the build context: {error}"))
        })?;
        let build_args = build_args(&self.image.components, versions);
        let tag = content_tag(&self.hash, base_id, &build_args);
        let request = BuildRequest {
            context: context.to_path_buf(),
            dockerfile: None,
            tag,
            build_args,
            labels: BTreeMap::from([
                (MANAGED_LABEL.to_string(), "true".to_string()),
                (DEFINITION_LABEL.to_string(), self.hash.clone()),
                (BUILT_AT_LABEL.to_string(), chrono::Utc::now().to_rfc3339()),
                (
                    CONTROLLER_LABEL.to_string(),
                    self.inner.controller_id.clone(),
                ),
            ]),
        };
        match self.inner.docker.build_image(&request) {
            Ok(info) => Ok(info.id),
            Err(error) => Err(build_failure(&self.image.components, &error)),
        }
    }
}

/// The failure of a build: the component whose layer failed, when the build
/// output names its script, else the build error. Only a daemon that did not
/// answer or a registry that could not be reached is retryable.
fn build_failure(components: &[TaskImageComponent], error: &DockerError) -> JobFailure {
    let text = error.to_string();
    if matches!(error, DockerError::Unreachable(_)) {
        return JobFailure {
            message: format!("the task image build failed: {text}"),
            retryable: true,
        };
    }
    // The failing step is named last (BuildKit ends with the failing
    // command and the Dockerfile excerpt).
    let failed = components
        .iter()
        .filter_map(|component| {
            text.rfind(&script_in_image(component))
                .map(|at| (at, component))
        })
        .max_by_key(|(at, _)| *at);
    match failed {
        Some((_, component)) => JobFailure::permanent(format!(
            "the install script of component '{}' failed while building the task image: {text}",
            component.name
        )),
        None => JobFailure {
            retryable: is_network_failure(&text),
            message: format!("the task image build failed: {text}"),
        },
    }
}

/// Whether a build error says the registry or the network could not be
/// reached (pulling the base, typically), which may pass on its own.
fn is_network_failure(text: &str) -> bool {
    const NETWORK: [&str; 11] = [
        "failed to resolve source metadata",
        "failed to do request",
        "dial tcp",
        "i/o timeout",
        "tls handshake timeout",
        "connection refused",
        "connection reset by peer",
        "temporary failure in name resolution",
        "no such host",
        "toomanyrequests",
        "503 service unavailable",
    ];
    let lower = text.to_lowercase();
    NETWORK.iter().any(|marker| lower.contains(marker))
}

/// The [`CONTROLLER_LABEL`] value of the controller whose directory is
/// `controller_dir`.
fn controller_id(controller_dir: &Path) -> String {
    let canonical = fs::canonicalize(controller_dir)
        .or_else(|_| std::path::absolute(controller_dir))
        .unwrap_or_else(|_| controller_dir.to_path_buf());
    let digest = hex(&Sha256::digest(canonical.as_os_str().as_encoded_bytes()));
    digest[..16].to_string()
}

/// Refuses what would change the Dockerfile's structure: a base with
/// whitespace, a component name outside `[a-z0-9][a-z0-9-]*`. The catalogue
/// validates both; this keeps the builder safe on its own.
fn check_definition(image: &TaskImage) -> Result<(), String> {
    let base = &image.base.reference;
    if base.is_empty() || base.chars().any(char::is_whitespace) {
        return Err(format!(
            "the task image base {base:?} is not an image reference"
        ));
    }
    for component in &image.components {
        let name = &component.name;
        let valid = name.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
            && name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if !valid {
            return Err(format!("{name:?} is not a task image component name"));
        }
    }
    Ok(())
}

/// The build argument of a component's layer.
fn version_arg(component: &TaskImageComponent) -> String {
    format!(
        "NARSOR_{}_VERSION",
        component.name.to_uppercase().replace('-', "_")
    )
}

fn script_file(component: &TaskImageComponent) -> String {
    format!("{}.sh", component.name)
}

fn script_in_image(component: &TaskImageComponent) -> String {
    format!("{SCRIPTS_IN_IMAGE}/{}", script_file(component))
}

/// One build argument per component, in order: its `version` output, empty
/// without one.
fn build_args(
    components: &[TaskImageComponent],
    versions: &BTreeMap<String, String>,
) -> Vec<(String, String)> {
    components
        .iter()
        .map(|component| {
            let value = versions.get(&component.name).cloned().unwrap_or_default();
            (version_arg(component), value)
        })
        .collect()
}

/// A tag that names the content: the definition, the base and the versions.
fn content_tag(hash: &str, base_id: Option<&str>, build_args: &[(String, String)]) -> String {
    let mut content = Sha256::new();
    content.update(base_id.unwrap_or_default());
    for (key, value) in build_args {
        content.update([0]);
        content.update(key);
        content.update([0]);
        content.update(value);
    }
    let content = hex(&content.finalize());
    format!("{TAG_REPOSITORY}:{}-{}", short(hash), &content[..12])
}

/// The Dockerfile of `image`: the base, a passwd and group entry for
/// `user`, then one layer per component.
fn dockerfile(image: &TaskImage, user: UserIds) -> String {
    let UserIds { uid, gid } = user;
    let mut file = format!("FROM {}\nUSER root\n", image.base.reference);
    let _ = writeln!(
        file,
        "RUN grep -q -E '^[^:]*:[^:]*:{gid}:' /etc/group || \
         echo '{IMAGE_USER}:x:{gid}:' >> /etc/group"
    );
    let _ = writeln!(
        file,
        "RUN grep -q -E '^[^:]*:[^:]*:{uid}:' /etc/passwd || \
         echo '{IMAGE_USER}:x:{uid}:{gid}:{IMAGE_USER}:/nonexistent:/bin/sh' >> /etc/passwd"
    );
    for component in &image.components {
        let arg = version_arg(component);
        let script = script_in_image(component);
        let _ = writeln!(file, "COPY {} {script}", script_file(component));
        let _ = writeln!(file, "ARG {arg}");
        let _ = writeln!(file, "RUN NARSOR_COMPONENT_VERSION=\"${arg}\" sh {script}");
    }
    file
}

/// Writes the Dockerfile and one script per component into `context`.
fn write_context(context: &Path, image: &TaskImage, user: UserIds) -> io::Result<()> {
    create_private_dir(context)?;
    for component in &image.components {
        fs::write(context.join(script_file(component)), &component.install)?;
    }
    fs::write(context.join("Dockerfile"), dockerfile(image, user))
}

/// Why a `version` script gave no output.
#[derive(Debug)]
enum ScriptError {
    Start(io::Error),
    TimedOut(Duration),
    Exited(std::process::ExitStatus),
}

impl std::fmt::Display for ScriptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Start(error) => write!(f, "cannot start: {error}"),
            Self::TimedOut(timeout) => write!(f, "did not end within {}s", timeout.as_secs_f32()),
            Self::Exited(status) => write!(f, "failed ({status})"),
        }
    }
}

/// Runs a `version` script with `sh -c` on the host, with only PATH, HOME
/// and LANG of the controller's environment; returns its trimmed output, at
/// most `cap` bytes of it.
fn run_version_script(script: &str, timeout: Duration, cap: usize) -> Result<String, ScriptError> {
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(script)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        // Its own process group, so a timeout kills what it started too.
        .process_group(0);
    for name in VERSION_SCRIPT_ENV {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    let mut child = command.spawn().map_err(ScriptError::Start)?;
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut output = Vec::new();
        let _ = (&mut stdout)
            .take(u64::try_from(cap).unwrap_or(u64::MAX))
            .read_to_end(&mut output);
        // Drain the rest so the script does not block on a full pipe.
        let _ = io::copy(&mut stdout, &mut io::sink());
        let _ = sender.send(output);
    });
    let output = match receiver.recv_timeout(timeout) {
        Ok(output) => output,
        Err(_) => {
            kill_group(&mut child);
            return Err(ScriptError::TimedOut(timeout));
        }
    };
    let status = child.wait().map_err(ScriptError::Start)?;
    if !status.success() {
        return Err(ScriptError::Exited(status));
    }
    Ok(String::from_utf8_lossy(&output).trim().to_string())
}

fn kill_group(child: &mut std::process::Child) {
    if let Ok(pid) = libc::pid_t::try_from(child.id()) {
        // SAFETY: a plain signal to the process group the child leads.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn create_private_dir(path: &Path) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

fn short(hash: &str) -> &str {
    &hash[..hash.len().min(12)]
}

fn to_ms(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH).map_or(0, |since| {
        u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
    })
}

fn from_ms(ms: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(ms)
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::Receiver;
    use std::time::Instant;

    use narsor_model::domain::ImageSpec;

    use super::super::docker_fake::{DockerCall, FakeDocker};
    use super::*;

    const BASE: &str = "debian:trixie-slim";

    /// A temporary directory removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "narsor-image-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4()
            ));
            fs::create_dir_all(&dir).expect("create the directory");
            Self(dir)
        }

        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// A test setup: a definition with a fixed component `a` and a
    /// component `b` whose `version` script counts its runs, prints the
    /// content of a file and sleeps while a flag file exists.
    struct Setup {
        temp: TempDir,
        docker: Arc<FakeDocker>,
        builds: TaskImageBuilds,
        image: TaskImage,
    }

    impl Setup {
        fn new(update_interval: Option<Duration>) -> Self {
            let temp = TempDir::new();
            let docker = Arc::new(FakeDocker::new());
            let builds = TaskImageBuilds::new(
                docker.clone(),
                &temp.path("controller"),
                settings(update_interval),
            );
            let version = format!(
                "echo run >> '{runs}'; if [ -e '{slow}' ]; then sleep 5; fi; cat '{version}'",
                runs = temp.path("runs").display(),
                slow = temp.path("slow").display(),
                version = temp.path("version").display(),
            );
            let image = TaskImage {
                base: ImageSpec::pinned(BASE),
                components: vec![component("a", None), component("b", Some(&version))],
                secrets: Vec::new(),
            };
            let setup = Self {
                temp,
                docker,
                builds,
                image,
            };
            setup.set_version("1.0");
            setup
        }

        fn set_version(&self, version: &str) {
            fs::write(self.temp.path("version"), format!("{version}\n")).unwrap();
        }

        fn version_runs(&self) -> usize {
            fs::read_to_string(self.temp.path("runs"))
                .map(|runs| runs.lines().count())
                .unwrap_or(0)
        }

        fn build_count(&self) -> usize {
            self.docker.builds().len()
        }

        /// Requests the image and waits for the job's outcome.
        fn request_and_wait(&self) -> BuildState {
            self.builds.request(&self.image);
            self.wait()
        }

        fn wait(&self) -> BuildState {
            wait_for(&self.builds, &self.image)
        }

        /// A builder of another controller on the same daemon.
        fn other_controller(&self, update_interval: Option<Duration>) -> TaskImageBuilds {
            TaskImageBuilds::new(
                self.docker.clone(),
                &self.temp.path("other-controller"),
                settings(update_interval),
            )
        }
    }

    fn settings(update_interval: Option<Duration>) -> BuildSettings {
        BuildSettings {
            update_interval,
            version_timeout: Duration::from_millis(300),
            version_output_cap: 64,
            user: UserIds {
                uid: 1000,
                gid: 100,
            },
            retry_delay: Duration::from_millis(100),
        }
    }

    /// Polls until the job of `image` ends.
    fn wait_for(builds: &TaskImageBuilds, image: &TaskImage) -> BuildState {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match builds.poll(image) {
                BuildState::Building if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                state => return state,
            }
        }
    }

    fn ready(builds: &TaskImageBuilds, image: &TaskImage) -> String {
        builds.request(image);
        match wait_for(builds, image) {
            BuildState::Ready(id) => id,
            other => panic!("expected an image, got {other:?}"),
        }
    }

    /// A second definition, with one component of its own.
    fn other_image() -> TaskImage {
        TaskImage {
            base: ImageSpec::pinned(BASE),
            components: vec![component("c", None)],
            secrets: Vec::new(),
        }
    }

    impl Setup {
        fn ready_id(&self) -> String {
            match self.request_and_wait() {
                BuildState::Ready(id) => id,
                other => panic!("expected an image, got {other:?}"),
            }
        }
    }

    fn component(name: &str, version: Option<&str>) -> TaskImageComponent {
        TaskImageComponent {
            name: name.into(),
            install: format!("echo install {name}\n"),
            version: version.map(str::to_string),
            secrets: Vec::new(),
            home: Vec::new(),
        }
    }

    #[test]
    fn a_built_image_is_reused_within_the_interval_and_checked_again_on_refresh() {
        let setup = Setup::new(None);

        let id = setup.ready_id();

        let pulls: Vec<_> = setup
            .docker
            .calls()
            .into_iter()
            .filter(|call| matches!(call, DockerCall::PullImage(_)))
            .collect();
        assert_eq!(pulls, [DockerCall::PullImage(BASE.into())]);
        let build = &setup.docker.builds()[0];
        assert_eq!(
            build.build_args,
            [
                ("NARSOR_A_VERSION".to_string(), String::new()),
                ("NARSOR_B_VERSION".to_string(), "1.0".to_string()),
            ]
        );
        assert_eq!(build.labels[MANAGED_LABEL], "true");
        assert_eq!(
            build.labels[DEFINITION_LABEL],
            definition_hash(&setup.image)
        );
        assert!(build.tag.starts_with("narsor-task-image:"));
        assert_eq!((setup.version_runs(), setup.build_count()), (1, 1));

        // Within the interval: no version check, no build.
        assert_eq!(
            setup.builds.request(&setup.image),
            BuildState::Ready(id.clone())
        );
        assert_eq!((setup.version_runs(), setup.build_count()), (1, 1));

        // A refresh checks again; the same versions keep the image.
        std::thread::sleep(Duration::from_millis(20));
        request_refresh(&setup.temp.path("controller")).unwrap();
        assert_eq!(setup.builds.request(&setup.image), BuildState::Building);
        assert_eq!(setup.wait(), BuildState::Ready(id));
        assert_eq!((setup.version_runs(), setup.build_count()), (2, 1));
    }

    #[test]
    fn a_new_version_after_the_interval_rebuilds_and_the_old_image_lives_while_referenced() {
        let setup = Setup::new(Some(Duration::ZERO));
        let old = setup.ready_id();

        setup.set_version("2.0");
        let new = setup.ready_id();

        assert_ne!(new, old);
        assert_eq!(setup.build_count(), 2);
        assert_eq!(
            setup.docker.builds()[1].build_args[1],
            ("NARSOR_B_VERSION".to_string(), "2.0".to_string())
        );

        let referenced = HashSet::from([old.clone()]);
        assert_eq!(setup.builds.collect_garbage(&referenced), Ok(0));
        assert!(setup.docker.image_ids().contains(&old));

        assert_eq!(setup.builds.collect_garbage(&HashSet::new()), Ok(1));
        let left = setup.docker.image_ids();
        assert!(!left.contains(&old));
        assert!(left.contains(&new), "the latest image stays");
        assert_eq!(left.len(), 2, "the unlabelled base stays too: {left:?}");
    }

    #[test]
    fn garbage_collection_leaves_the_images_of_another_controller_alone() {
        let setup = Setup::new(Some(Duration::ZERO));
        setup.ready_id();
        let other = setup.other_controller(Some(Duration::ZERO));
        let superseded = ready(&other, &setup.image);
        setup.set_version("2.0");
        let latest = ready(&other, &setup.image);

        assert_eq!(setup.builds.collect_garbage(&HashSet::new()), Ok(0));
        assert!(setup.docker.image_ids().contains(&superseded));

        assert_eq!(other.collect_garbage(&HashSet::new()), Ok(1));
        let left = setup.docker.image_ids();
        assert!(!left.contains(&superseded));
        assert!(left.contains(&latest));
    }

    #[test]
    fn a_definition_out_of_use_loses_its_image_and_cache_entry() {
        let setup = Setup::new(Some(Duration::ZERO));
        let referenced = setup.ready_id();
        let unused = ready(&setup.builds, &other_image());
        let inner = &setup.builds.inner;
        let hashes = [
            definition_hash(&setup.image),
            definition_hash(&other_image()),
        ];
        // Both were last checked and requested long ago.
        for hash in &hashes {
            let entry = CacheEntry {
                checked_at_ms: 0,
                ..inner.read_cache(hash)
            };
            inner.write_cache(hash, &entry).unwrap();
            fs::File::options()
                .write(true)
                .open(inner.requested_path(hash))
                .unwrap()
                .set_modified(UNIX_EPOCH)
                .unwrap();
        }

        let removed = setup
            .builds
            .collect_garbage(&HashSet::from([referenced.clone()]));

        assert_eq!(removed, Ok(1));
        let left = setup.docker.image_ids();
        assert!(left.contains(&referenced), "its definition is in use");
        assert!(!left.contains(&unused));
        assert!(inner.cache_path(&hashes[0]).exists());
        assert!(!inner.cache_path(&hashes[1]).exists());
    }

    #[test]
    fn a_slow_docker_call_for_one_definition_does_not_hold_up_another() {
        let setup = Setup::new(None);
        let id = setup.ready_id();
        let (entered, wait_entered) = mpsc::channel::<()>();
        let (release, wait_release) = mpsc::channel::<()>();
        let hook = Mutex::new((entered, wait_release));
        setup.docker.on_call(move |call| {
            if matches!(call, DockerCall::ListImages(_)) {
                let hook = hook.lock().unwrap();
                let _ = hook.0.send(());
                let _ = hook.1.recv();
            }
        });
        let collector = setup.builds.clone();
        let collection = std::thread::spawn(move || collector.collect_garbage(&HashSet::new()));
        wait_entered.recv().unwrap();

        // Asked from another thread, so a regression fails instead of hanging.
        let (answer, answers) = mpsc::channel();
        let builds = setup.builds.clone();
        let image = setup.image.clone();
        std::thread::spawn(move || {
            let other = builds.request(&other_image());
            let known = builds.poll(&image);
            let _ = answer.send((other, known));
        });
        let answered = answers.recv_timeout(Duration::from_secs(2));

        release.send(()).unwrap();
        assert_eq!(collection.join().unwrap(), Ok(0));
        assert_eq!(
            answered,
            Ok((BuildState::Building, BuildState::Ready(id))),
            "answered while the collection waited on Docker"
        );
        assert!(matches!(
            wait_for(&setup.builds, &other_image()),
            BuildState::Ready(_)
        ));
    }

    #[test]
    fn a_refresh_asked_for_during_a_failing_build_is_not_lost() {
        let setup = Setup::new(Some(Duration::from_secs(3600)));
        setup.docker.fail_builds(Some("exit code: 1"));
        let (release, wait_release) = mpsc::channel::<()>();
        let wait_release = Mutex::new(wait_release);
        setup.docker.on_build(move |_| {
            let _ = wait_release.lock().unwrap().recv();
        });
        assert_eq!(setup.builds.request(&setup.image), BuildState::Building);

        std::thread::sleep(Duration::from_millis(20));
        request_refresh(&setup.temp.path("controller")).unwrap();
        release.send(()).unwrap();
        release.send(()).unwrap();
        assert!(matches!(setup.wait(), BuildState::Failed(_)));

        assert_eq!(setup.builds.request(&setup.image), BuildState::Building);
        assert!(matches!(setup.wait(), BuildState::Failed(_)));
        assert_eq!(setup.build_count(), 2);
    }

    #[test]
    fn only_a_daemon_or_registry_that_did_not_answer_makes_a_build_retryable() {
        let components = [component("glab", None)];
        let failed = |message: &str| DockerError::Failed {
            command: "docker build".into(),
            message: message.into(),
        };
        let cases = [
            (
                DockerError::Unreachable("`docker build` did not answer within 900s".into()),
                true,
            ),
            (
                failed(
                    "failed to resolve source metadata for docker.io/library/debian: dial tcp: \
                     lookup registry-1.docker.io: Temporary failure in name resolution",
                ),
                true,
            ),
            (
                failed(
                    "process \"/bin/sh -c sh /opt/narsor/components/glab.sh\" did not complete \
                     successfully: exit code: 1",
                ),
                false,
            ),
            (
                failed("dockerfile parse error on line 3: unknown instruction"),
                false,
            ),
        ];
        for (error, retryable) in cases {
            assert_eq!(
                build_failure(&components, &error).retryable,
                retryable,
                "{error}"
            );
        }
    }

    #[test]
    fn a_failed_build_keeps_the_previous_image_or_names_the_failing_component() {
        let failure = "process \"/bin/sh -c NARSOR_COMPONENT_VERSION=\\\"$NARSOR_B_VERSION\\\" \
                       sh /opt/narsor/components/b.sh\" did not complete successfully: exit code: 1";

        // With a previous image: it stays in use.
        let setup = Setup::new(Some(Duration::ZERO));
        let previous = setup.ready_id();
        setup.set_version("2.0");
        setup.docker.fail_builds(Some(failure));
        assert_eq!(setup.request_and_wait(), BuildState::Ready(previous));

        // Without one: the failure names the component, and polling does
        // not build again.
        let setup = Setup::new(Some(Duration::from_secs(3600)));
        setup.docker.fail_builds(Some(failure));
        let BuildState::Failed(message) = setup.request_and_wait() else {
            panic!("expected a failure");
        };
        assert!(message.contains("component 'b'"), "{message}");
        assert!(matches!(
            setup.builds.poll(&setup.image),
            BuildState::Failed(_)
        ));
        assert!(matches!(
            setup.builds.request(&setup.image),
            BuildState::Failed(_)
        ));
        assert_eq!(setup.build_count(), 1);
    }

    #[test]
    fn a_version_script_that_times_out_keeps_the_previous_version() {
        let setup = Setup::new(Some(Duration::ZERO));
        let id = setup.ready_id();
        fs::write(setup.temp.path("slow"), "").unwrap();
        setup.set_version("2.0");

        let started = Instant::now();
        assert_eq!(setup.request_and_wait(), BuildState::Ready(id));

        assert!(
            started.elapsed() < Duration::from_secs(4),
            "the script was killed"
        );
        assert_eq!(setup.build_count(), 1, "1.0 is still the version");
    }

    #[test]
    fn concurrent_requests_for_one_definition_share_one_job() {
        let setup = Setup::new(None);
        let (release, wait_release) = mpsc::channel::<()>();
        let wait_release: Mutex<Receiver<()>> = Mutex::new(wait_release);
        setup.docker.on_build(move |_| {
            let _ = wait_release.lock().unwrap().recv();
        });

        let states: Vec<BuildState> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..2)
                .map(|_| scope.spawn(|| setup.builds.request(&setup.image)))
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert_eq!(states, [BuildState::Building, BuildState::Building]);
        // The build blocks; another request still answers at once.
        let started = Instant::now();
        assert_eq!(setup.builds.request(&setup.image), BuildState::Building);
        assert!(started.elapsed() < Duration::from_secs(1));

        release.send(()).unwrap();
        assert!(matches!(setup.wait(), BuildState::Ready(_)));
        assert_eq!(setup.build_count(), 1);
    }

    #[test]
    fn a_version_script_sees_no_controller_environment_and_its_output_is_capped() {
        let names = run_version_script(
            "env | cut -d= -f1 | grep -v -x -e PATH -e HOME -e LANG -e PWD -e SHLVL -e _ | \
             tr '\\n' ' '",
            Duration::from_secs(5),
            1024,
        )
        .unwrap();
        assert_eq!(names, "", "unexpected variables");

        let long = run_version_script("printf '  %0200d\\n' 7", Duration::from_secs(5), 16);
        assert_eq!(long.unwrap(), "00000000000000", "16 bytes, trimmed");

        assert!(run_version_script("echo 1.0; exit 3", Duration::from_secs(5), 64).is_err());
    }

    #[test]
    fn the_dockerfile_adds_the_host_user_then_one_layer_per_component() {
        let image = TaskImage {
            base: ImageSpec::pinned(BASE),
            components: vec![component("node-js", Some("echo 22")), component("uv", None)],
            secrets: Vec::new(),
        };

        let file = dockerfile(
            &image,
            UserIds {
                uid: 1000,
                gid: 100,
            },
        );

        let lines: Vec<&str> = file.lines().collect();
        assert_eq!(
            lines,
            [
                "FROM debian:trixie-slim",
                "USER root",
                "RUN grep -q -E '^[^:]*:[^:]*:100:' /etc/group || \
                 echo 'narsor:x:100:' >> /etc/group",
                "RUN grep -q -E '^[^:]*:[^:]*:1000:' /etc/passwd || \
                 echo 'narsor:x:1000:100:narsor:/nonexistent:/bin/sh' >> /etc/passwd",
                "COPY node-js.sh /opt/narsor/components/node-js.sh",
                "ARG NARSOR_NODE_JS_VERSION",
                "RUN NARSOR_COMPONENT_VERSION=\"$NARSOR_NODE_JS_VERSION\" \
                 sh /opt/narsor/components/node-js.sh",
                "COPY uv.sh /opt/narsor/components/uv.sh",
                "ARG NARSOR_UV_VERSION",
                "RUN NARSOR_COMPONENT_VERSION=\"$NARSOR_UV_VERSION\" sh /opt/narsor/components/uv.sh",
            ]
        );
    }
}
