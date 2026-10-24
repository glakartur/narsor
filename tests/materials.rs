//! Integration tests for bounded task materials (U9: R23, R30, AE7).
//!
//! The control plane is the real daemon API ([`narsor::api::start_with_services`])
//! on a loopback port it bound itself, serving the owner routes behind the
//! trusted token and the runner routes behind runner credentials. The
//! controller side (enrollment, lease, reports, runner credentials) is
//! driven in-process. A material's way through the control plane is also
//! driven in-process, with the runner's side played by its credential
//! (`a_materials_way_through_the_control_plane_carries_no_content`); what the
//! runner does with a material on its task storage is tested in the
//! `narsor-runner` crate (`runner::materials`).

mod environment_support;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use chrono::Utc;
use narsor_api_client::api::{ApiClient, ClientError};
use narsor_engine::controller_protocol::{
    AcquireLeaseRequest, ControllerIdentity, CreateEnrollmentRequest, OwnerCommandRequest,
    ProtocolError, ProtocolService, ReportRequest, RunnerClient, RunnerCredentialRequest,
    RunnerPollQuery, LOCAL_OWNER,
};
use narsor_engine::domain::{
    BackendId, GitIdentity, ResolvedEnvironment, ResourceProfile, TaskId, TaskLaunchSpec,
    TaskWorkspace,
};
use narsor_engine::engine::Engine;
use narsor_engine::materials::{
    sha256_hex, snapshot_paths, write_files, Bundle, MaterialAckRequest, MaterialFile,
    MaterialLimits, MaterialOrigin, UploadManifest, WriteMode,
};
use narsor_engine::orchestration::{
    ControlPlane, MaterialChange, NewTask, Observation, ObservedSummary, OwnerCommand, OwnerRef,
    TaskEventKind,
};
use narsor_engine::persistence::paths::StatePaths;
use narsor_engine::persistence::ExecutionStore;
use narsor_server::api::{self, ApiHandle, ApiServices};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

const BACKEND: &str = "k8s";

/// Text that appears only inside uploaded files: finding it anywhere but in
/// a material blob or a delivered copy means content leaked.
const SECRET_CONTENT: &str = "material-content-7f3c9a-never-in-events";

/// A file name that appears only inside manifests.
const SECRET_NAME: &str = "only-in-the-manifest-4b1e.md";

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("narsor-materials-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).expect("create temp root");
        Self { path }
    }

    fn dir(&self, name: &str) -> PathBuf {
        let path = self.path.join(name);
        fs::create_dir_all(&path).expect("create directory");
        path
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Small bounds, so oversize submissions are cheap to build.
fn test_limits() -> MaterialLimits {
    MaterialLimits {
        max_file_bytes: 4096,
        max_total_bytes: 8192,
        max_published_bytes: 4096,
        max_files: 8,
        max_manifest_bytes: 64 * 1024,
        max_materials_per_task: 4,
        max_task_bytes: 16 * 1024,
        ..MaterialLimits::default()
    }
}

struct Fixture {
    root: TempRoot,
    state: PathBuf,
    service: Arc<ProtocolService>,
    /// The daemon API serving the service; `None` for a fixture whose
    /// scenario calls the service in-process only.
    handle: Option<ApiHandle>,
    controller: ControllerIdentity,
    epoch: u64,
}

impl Fixture {
    /// A fixture served by the daemon API on a loopback port.
    async fn new() -> Self {
        let mut fixture = Self::in_process();
        // The server binds 127.0.0.1:0 itself and keeps the listener.
        let handle = api::start_with_services(
            Arc::new(Engine::new(ExecutionStore::new(
                fixture.root.dir("local-engine"),
            ))),
            fixture.state.clone(),
            fixture.state.clone(),
            ApiServices {
                control: Some(Arc::clone(&fixture.service)),
                ..ApiServices::default()
            },
        )
        .await
        .expect("the API starts");
        fixture.handle = Some(handle);
        fixture
    }

    /// A fixture whose service is called in-process only.
    fn in_process() -> Self {
        let root = TempRoot::new();
        let state = root.dir("control");
        let control = ControlPlane::new(StatePaths::new(&state));
        let service = Arc::new(
            ProtocolService::new(control, environment_support::seeded_resolver(&state))
                .with_material_limits(test_limits()),
        );
        let code = service
            .create_enrollment(BACKEND, CreateEnrollmentRequest::default())
            .expect("enrollment");
        let enrolled = service
            .enroll(narsor_engine::auth::pop::signed_enrollment_request(
                &code.enrollment_code,
                &narsor_engine::auth::ControllerKey::generate(),
            ))
            .expect("enroll");
        let controller = ControllerIdentity {
            controller_id: enrolled.controller_id,
            backend_id: enrolled.backend_id,
        };
        let epoch = service
            .acquire_lease(
                &controller,
                AcquireLeaseRequest {
                    ttl_secs: Some(600),
                },
            )
            .expect("lease")
            .epoch;
        Self {
            root,
            state,
            service,
            handle: None,
            controller,
            epoch,
        }
    }

    fn handle(&self) -> &ApiHandle {
        self.handle.as_ref().expect("a fixture served by the API")
    }

    fn client(&self) -> ApiClient {
        self.handle().client()
    }

    fn url(&self) -> String {
        format!("http://{}", self.handle().addr())
    }

    fn owner(&self) -> OwnerRef {
        OwnerRef::new(LOCAL_OWNER)
    }

    fn create(&self, task: &str, spec: TaskLaunchSpec) {
        self.service
            .control_plane()
            .create_task(NewTask::new(
                TaskId::parse(task).unwrap(),
                self.owner(),
                BackendId::parse(BACKEND).unwrap(),
                spec,
            ))
            .expect("create the task");
    }

    /// Creates a task and brings it to `Provisioning`, where a runner may
    /// be launched.
    fn create_started(&self, task: &str, spec: TaskLaunchSpec) {
        self.create(task, spec);
        self.report(task, Observation::StartAccepted);
    }

    fn report(&self, task: &str, observation: Observation) {
        let generation = self
            .service
            .control_plane()
            .task(&TaskId::parse(task).unwrap())
            .unwrap()
            .generation;
        self.service
            .report(
                &self.controller,
                ReportRequest {
                    task_id: task.to_string(),
                    lease_epoch: self.epoch,
                    desired_generation: generation,
                    observation,
                    resource: None,
                    summary: ObservedSummary::default(),
                    observed_at: Utc::now(),
                },
            )
            .unwrap_or_else(|error| panic!("report {observation:?}: {error}"));
    }

    fn command(&self, task: &str, command: OwnerCommand) {
        self.service
            .command(
                &self.owner(),
                task,
                OwnerCommandRequest {
                    command,
                    idempotency_key: uuid::Uuid::new_v4().to_string(),
                    expected_generation: None,
                },
            )
            .unwrap_or_else(|error| panic!("{command:?}: {error}"));
    }

    fn credential(&self, task: &str, incarnation: &str) -> String {
        let generation = self
            .service
            .control_plane()
            .task(&TaskId::parse(task).unwrap())
            .unwrap()
            .generation;
        self.service
            .issue_runner_credential(
                &self.controller,
                task,
                RunnerCredentialRequest {
                    lease_epoch: self.epoch,
                    generation,
                    incarnation: incarnation.to_string(),
                },
            )
            .expect("runner credential")
            .credential
    }

    fn runner_client(&self, task: &str, incarnation: &str) -> RunnerClient {
        RunnerClient::new(self.url(), self.credential(task, incarnation))
    }

    /// Every file below the material store except the lock files.
    fn stored_files(&self) -> Vec<PathBuf> {
        files_below(&self.state.join("materials"))
            .into_iter()
            .filter(|path| !path.components().any(|c| c.as_os_str() == ".locks"))
            .collect()
    }

    fn events(&self, task: &str) -> Vec<narsor_engine::orchestration::TaskEvent> {
        self.service
            .events(&self.owner(), task, 0)
            .expect("task events")
    }
}

fn files_below(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let file_type = entry.file_type().unwrap();
        if file_type.is_dir() {
            out.extend(files_below(&path));
        } else {
            out.push(path);
        }
    }
    out.sort();
    out
}

/// Whether any file below `dir` contains `needle`.
fn contains_anywhere(dir: &Path, needle: &str) -> Vec<PathBuf> {
    files_below(dir)
        .into_iter()
        .filter(|path| {
            fs::read(path).is_ok_and(|bytes| {
                bytes
                    .windows(needle.len())
                    .any(|window| window == needle.as_bytes())
            })
        })
        .collect()
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
        .args(args)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// A bare "remote" with one commit, and that commit's id.
fn origin(root: &TempRoot) -> (PathBuf, String) {
    let work = root.dir("origin-work");
    git(&work, &["-c", "init.defaultBranch=main", "init", "-q"]);
    fs::write(work.join("README.md"), "pinned\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "pinned"]);
    let bare = root.path.join("origin.git");
    let status = Command::new("git")
        .args(["clone", "-q", "--bare"])
        .arg(&work)
        .arg(&bare)
        .status()
        .unwrap();
    assert!(status.success());
    (bare, git(&work, &["rev-parse", "HEAD"]))
}

/// The workflow of the tasks here; no runner runs it.
const OUTBOX_WORKFLOW: &str = r#"
name: materials-task
version: 1
states:
  write:
    action:
      type: script
      command: sh
      args:
        - -c
        - |
          mkdir -p "$NARSOR_OUTBOX_DIR/report"
          printf 'temporary documentation' > "$NARSOR_OUTBOX_DIR/report/notes.md"
          printf '{"choice": "done"}'
    choices: [done]
    transitions:
      done: gate
  gate:
    action:
      type: script
      command: sh
      args: [-c, "printf '{\"choice\": \"ok\"}'"]
    choices: [ok, rejected]
    transitions:
      ok: completed
      rejected: failed
    decision:
      type: human
      message: Ship it?
  completed:
    description: Done.
  failed:
    description: Rejected.
terminal_states:
  completed: completed
  failed: failed
"#;

fn spec(workspace: TaskWorkspace) -> TaskLaunchSpec {
    let definition =
        narsor_engine::workflow::load_from_str(OUTBOX_WORKFLOW).expect("a valid workflow");
    TaskLaunchSpec::new(
        definition.id.clone(),
        definition.version,
        serde_json::json!({}),
        workspace,
        ResolvedEnvironment::harness_only("claude-code"),
        ResourceProfile::new(1000, 2048),
    )
    .with_definition(&definition)
    .expect("snapshot the definition")
}

fn git_spec(root: &TempRoot) -> (TaskLaunchSpec, String) {
    let (bare, pinned) = origin(root);
    let spec = spec(TaskWorkspace::Git(GitIdentity {
        repository: Some("demo".to_string()),
        remote_url: bare.display().to_string(),
        commit: pinned.clone(),
        git_ref: None,
        ref_origin: None,
        credential: None,
    }));
    (spec, pinned)
}

/// A valid bundle of `files` (path, content).
fn bundle(id: Option<&str>, files: &[(&str, &[u8])]) -> Bundle {
    let mut blobs = BTreeMap::new();
    let manifest_files = files
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
    Bundle::new(
        UploadManifest {
            material_id: id.map(str::to_string),
            label: None,
            ttl_secs: None,
            files: manifest_files,
        },
        blobs,
    )
}

/// A raw bundle body from manifest JSON and blob bytes, for submissions
/// [`Bundle::encode`] would never produce.
fn raw_body(manifest: &serde_json::Value, blobs: &[u8]) -> Bytes {
    let manifest = serde_json::to_vec(manifest).unwrap();
    let mut body = b"NMB1".to_vec();
    body.extend_from_slice(&(manifest.len() as u32).to_be_bytes());
    body.extend_from_slice(&manifest);
    body.extend_from_slice(blobs);
    body.into()
}

fn refused_code(result: Result<impl std::fmt::Debug, ClientError>) -> String {
    match result {
        Err(ClientError::Request { code, .. }) | Err(ClientError::Refused { code, .. }) => code,
        other => panic!("expected a typed refusal, got {other:?}"),
    }
}

fn in_process(body: Bytes) -> futures::stream::Iter<std::vec::IntoIter<Result<Bytes, String>>> {
    futures::stream::iter(vec![Ok(body)])
}

// ---------------------------------------------------------------------------
// Upload, delivery, publication, archive (AE7), in-process
// ---------------------------------------------------------------------------

/// A material's way through the control plane, with the runner's side
/// played by its credential: the owner's snapshot of a local folder is
/// announced to the task's runner and fetched as it was uploaded, a
/// document the runner publishes comes back to the owner, the events carry
/// ids and counts only, and an archive drops it all.
#[tokio::test]
async fn a_materials_way_through_the_control_plane_carries_no_content() {
    let fixture = Fixture::in_process();
    let owner = fixture.owner();
    fixture.create_started("task-1", spec(TaskWorkspace::None));

    // The owner snapshots a local folder: two files, a link that must not
    // be followed and a nested `.git` that must not travel.
    let source = fixture.root.dir("local/brief");
    fs::create_dir_all(source.join("docs")).unwrap();
    fs::write(source.join("docs").join(SECRET_NAME), SECRET_CONTENT).unwrap();
    fs::write(source.join("plan.txt"), "step one\n").unwrap();
    fs::create_dir_all(source.join(".git")).unwrap();
    fs::write(source.join(".git/config"), "[core]\n").unwrap();
    let outside = fixture.root.dir("outside");
    fs::write(outside.join("private.key"), "do not send").unwrap();
    std::os::unix::fs::symlink(outside.join("private.key"), source.join("key-link")).unwrap();
    let snapshot = snapshot_paths(std::slice::from_ref(&source), &MaterialLimits::default())
        .expect("snapshot");
    let skipped: BTreeSet<String> = snapshot
        .skipped
        .iter()
        .map(|entry| {
            entry
                .path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(
        skipped,
        BTreeSet::from([".git".to_string(), "key-link".to_string()]),
        "the link and the Git metadata are skipped and reported"
    );
    let mut upload = snapshot.bundle;
    upload.manifest.material_id = Some("brief-1".to_string());
    let uploaded = fixture
        .service
        .upload_material(
            &owner,
            "task-1",
            in_process(upload.encode().unwrap().into()),
        )
        .await
        .expect("upload");
    assert!(!uploaded.replayed);
    assert_eq!(uploaded.material.file_count, 2);

    // The runner is told of it and fetches exactly the validated manifest.
    let credential = fixture.credential("task-1", "pod-1");
    let runner = fixture
        .service
        .authenticate_runner(&credential)
        .expect("the runner's identity");
    let poll = fixture
        .service
        .runner_poll(
            &runner,
            RunnerPollQuery {
                wait_secs: Some(0),
                ..RunnerPollQuery::default()
            },
        )
        .await
        .expect("poll");
    assert_eq!(poll.materials.len(), 1);
    assert_eq!(poll.materials[0].material_id, "brief-1");
    let view = fixture
        .service
        .runner_material(&runner, "brief-1")
        .expect("the manifest");
    let mut fetched = BTreeMap::new();
    for file in &view.files {
        let blob = fixture
            .service
            .runner_material_blob(&runner, "brief-1", &file.sha256)
            .expect("a blob");
        fetched.insert(file.path.clone(), fs::read(blob).unwrap());
    }
    let expected: BTreeMap<String, Vec<u8>> = [
        (
            format!("brief/docs/{SECRET_NAME}"),
            SECRET_CONTENT.as_bytes().to_vec(),
        ),
        ("brief/plan.txt".to_string(), b"step one\n".to_vec()),
    ]
    .into_iter()
    .collect();
    assert_eq!(fetched, expected, "exactly the manifest, nothing more");

    // Acknowledged twice (a lost answer): delivered once.
    let ack = || MaterialAckRequest {
        manifest_digest: poll.materials[0].manifest_digest.clone(),
    };
    let first = fixture
        .service
        .runner_ack_material(&runner, "brief-1", ack())
        .expect("ack");
    let second = fixture
        .service
        .runner_ack_material(&runner, "brief-1", ack())
        .expect("ack again");
    assert_eq!(first.delivered_at, second.delivered_at);

    // The document the runner publishes from the task's outbox comes back.
    let mut document = bundle(
        Some("out-report"),
        &[("notes.md", b"temporary documentation")],
    );
    document.manifest.label = Some("report".to_string());
    let published = fixture
        .service
        .runner_publish_material(&runner, in_process(document.encode().unwrap().into()))
        .await
        .expect("publish");
    assert_eq!(published.material.origin, MaterialOrigin::Runner);
    assert_eq!(published.material.label.as_deref(), Some("report"));

    // Events carry ids and counts only.
    let changes: Vec<MaterialChange> = fixture
        .events("task-1")
        .into_iter()
        .filter_map(|event| match event.kind {
            TaskEventKind::MaterialChanged { change, .. } => Some(change),
            _ => None,
        })
        .collect();
    for change in [
        MaterialChange::Uploaded,
        MaterialChange::Delivered,
        MaterialChange::Published,
    ] {
        assert!(changes.contains(&change), "{change:?} in {changes:?}");
    }
    assert_eq!(
        changes
            .iter()
            .filter(|change| **change == MaterialChange::Delivered)
            .count(),
        1
    );
    let events = serde_json::to_string(&fixture.events("task-1")).unwrap();
    let summary = serde_json::to_string(&fixture.service.task(&owner, "task-1").unwrap()).unwrap();
    for text in [&events, &summary] {
        assert!(!text.contains(SECRET_CONTENT));
        assert!(!text.contains(SECRET_NAME));
    }
    assert!(contains_anywhere(&fixture.state.join("orchestration"), SECRET_CONTENT).is_empty());
    assert!(contains_anywhere(&fixture.state.join("orchestration"), SECRET_NAME).is_empty());

    // Archive: the task's materials are gone, and a late submission for the
    // stale task is refused without leaving anything.
    fixture.command("task-1", OwnerCommand::Archive);
    assert!(fixture
        .service
        .list_materials(&owner, "task-1")
        .unwrap()
        .materials
        .is_empty());
    assert!(
        fixture.stored_files().is_empty(),
        "{:?}",
        fixture.stored_files()
    );
    let late = fixture
        .service
        .upload_material(
            &owner,
            "task-1",
            in_process(
                bundle(None, &[("late.txt", b"late")])
                    .encode()
                    .unwrap()
                    .into(),
            ),
        )
        .await;
    assert!(
        matches!(
            late,
            Err(ProtocolError::Conflict {
                code: "task_archived",
                ..
            })
        ),
        "{late:?}"
    );
    assert!(fixture.stored_files().is_empty());
}

// ---------------------------------------------------------------------------
// The runner channel, in-process
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_runner_channel_announces_until_acknowledged_and_acknowledging_twice_is_harmless() {
    let fixture = Fixture::new().await;
    let (spec, _) = git_spec(&fixture.root);
    fixture.create_started("task-1", spec.clone());
    fixture.create_started("task-2", spec);
    let upload = fixture
        .client()
        .upload_material(
            "task-1",
            bundle(None, &[("x/y.txt", b"why")])
                .encode()
                .unwrap()
                .into(),
        )
        .await
        .expect("upload");
    let id = upload.material.material_id;
    let digest = upload.material.manifest_digest;

    let runner = fixture.runner_client("task-1", "pod-1");
    let poll = runner.poll(None, 0).await.expect("poll");
    assert_eq!(poll.materials.len(), 1);
    assert_eq!(poll.materials[0].material_id, id);
    let view = runner.material(&id).await.expect("manifest");
    assert_eq!(view.files[0].path, "x/y.txt");
    let bytes = runner
        .material_blob(&id, &view.files[0].sha256)
        .await
        .expect("blob");
    assert_eq!(&bytes[..], b"why");

    // Still announced: nothing was acknowledged.
    assert_eq!(runner.poll(None, 0).await.unwrap().materials.len(), 1);
    let wrong = runner.ack_material(&id, "sha256:0000").await;
    assert_eq!(refused_code(wrong), "material_digest_mismatch");
    let first = runner.ack_material(&id, &digest).await.expect("ack");
    let second = runner.ack_material(&id, &digest).await.expect("ack again");
    assert_eq!(first.delivered_at, second.delivered_at);
    assert!(runner.poll(None, 0).await.unwrap().materials.is_empty());

    // Another task's runner sees none of it.
    let other = fixture.runner_client("task-2", "pod-2");
    assert!(other.poll(None, 0).await.unwrap().materials.is_empty());
    assert!(matches!(
        other.material(&id).await,
        Err(ClientError::NotFound)
    ));
    assert!(matches!(
        other.material_blob(&id, &view.files[0].sha256).await,
        Err(ClientError::NotFound)
    ));

    // A published document is bounded like an upload, but smaller.
    let too_big = vec![b'x'; 4097];
    let refused = runner
        .publish_material(bundle(None, &[("big.bin", &too_big)]).encode().unwrap())
        .await;
    assert_eq!(refused_code(refused), "material_too_large");
}

// ---------------------------------------------------------------------------
// Refusals leave nothing behind
// ---------------------------------------------------------------------------

#[tokio::test]
async fn invalid_submissions_are_refused_without_leaving_blobs_or_manifests() {
    let fixture = Fixture::new().await;
    let (spec, _) = git_spec(&fixture.root);
    fixture.create("task-1", spec);
    let client = fixture.client();
    let big = vec![b'x'; 4097];
    let half = vec![b'y'; 3000];
    let sha_of_a = sha256_hex(b"a");

    let cases: Vec<(&str, Bytes, &str)> = vec![
        (
            "a file over the per-file bound",
            bundle(None, &[("big.bin", &big)]).encode().unwrap().into(),
            "material_too_large",
        ),
        (
            "files over the total bound",
            bundle(
                None,
                &[("1", &half[..]), ("2", &half[..2999]), ("3", &half[..2998])],
            )
            .encode()
            .unwrap()
            .into(),
            "material_too_large",
        ),
        (
            "too many files",
            {
                let names: Vec<String> = (0..9).map(|n| format!("f{n}")).collect();
                let files: Vec<(&str, &[u8])> =
                    names.iter().map(|n| (n.as_str(), &b"z"[..])).collect();
                bundle(None, &files).encode().unwrap().into()
            },
            "material_too_large",
        ),
        (
            "a traversal",
            bundle(None, &[("../escape.txt", b"a")])
                .encode()
                .unwrap()
                .into(),
            "invalid_material_path",
        ),
        (
            "an absolute path",
            bundle(None, &[("/etc/passwd", b"a")])
                .encode()
                .unwrap()
                .into(),
            "invalid_material_path",
        ),
        (
            "a backslash trick",
            bundle(None, &[("a\\..\\..\\b", b"a")])
                .encode()
                .unwrap()
                .into(),
            "invalid_material_path",
        ),
        (
            "an empty component",
            bundle(None, &[("a//b", b"a")]).encode().unwrap().into(),
            "invalid_material_path",
        ),
        (
            "a NUL byte",
            bundle(None, &[("a\0b", b"a")]).encode().unwrap().into(),
            "invalid_material_path",
        ),
        (
            "Git metadata",
            bundle(None, &[(".git/hooks/post-checkout", b"a")])
                .encode()
                .unwrap()
                .into(),
            "invalid_material_path",
        ),
        (
            "case-colliding duplicates",
            bundle(None, &[("Readme.md", b"a"), ("README.md", b"b")])
                .encode()
                .unwrap()
                .into(),
            "invalid_material_path",
        ),
        (
            "a file that is also a directory",
            bundle(None, &[("a", b"a"), ("a/b", b"b")])
                .encode()
                .unwrap()
                .into(),
            "invalid_material_path",
        ),
        (
            "a symbolic link entry",
            raw_body(
                &serde_json::json!({"files": [{
                    "path": "link", "size": 1, "sha256": sha_of_a, "symlink_target": "/etc"
                }]}),
                b"a",
            ),
            "invalid_material_manifest",
        ),
        (
            "content that does not hash to its digest",
            raw_body(
                &serde_json::json!({"files": [{"path": "a.txt", "size": 1, "sha256": sha_of_a}]}),
                b"b",
            ),
            "material_digest_mismatch",
        ),
        (
            "a truncated upload",
            {
                let mut body = bundle(None, &[("a.txt", b"abcdef")]).encode().unwrap();
                body.truncate(body.len() - 2);
                body.into()
            },
            "material_incomplete",
        ),
        (
            "bytes beyond the manifest",
            {
                let mut body = bundle(None, &[("a.txt", b"abcdef")]).encode().unwrap();
                body.extend_from_slice(b"extra");
                body.into()
            },
            "material_incomplete",
        ),
    ];
    for (what, body, code) in cases {
        let result = client.upload_material("task-1", body).await;
        assert_eq!(refused_code(result), code, "{what}");
        assert!(
            fixture.stored_files().is_empty(),
            "{what} left {:?}",
            fixture.stored_files()
        );
    }
    assert!(client
        .materials("task-1")
        .await
        .unwrap()
        .materials
        .is_empty());
    assert!(
        fixture
            .events("task-1")
            .iter()
            .all(|event| !matches!(event.kind, TaskEventKind::MaterialChanged { .. })),
        "a refusal leaves no event either"
    );

    // An upload that is not a bundle at all is refused before parsing.
    let raw = reqwest::Client::new()
        .post(format!(
            "{}/control/v1/tasks/task-1/materials",
            fixture.url()
        ))
        .bearer_auth(fixture.handle().token())
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(raw.status(), reqwest::StatusCode::BAD_REQUEST);
    // And no token, no materials.
    let anonymous = reqwest::Client::new()
        .get(format!(
            "{}/control/v1/tasks/task-1/materials",
            fixture.url()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(anonymous.status(), reqwest::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn quotas_and_reused_ids_are_refused_and_an_identical_retry_is_a_replay() {
    let fixture = Fixture::new().await;
    let (spec, _) = git_spec(&fixture.root);
    fixture.create("task-1", spec);
    let client = fixture.client();

    let first = bundle(Some("m-1"), &[("a.txt", b"one")]);
    let created = client
        .upload_material("task-1", first.encode().unwrap().into())
        .await
        .unwrap();
    let replay = client
        .upload_material("task-1", first.encode().unwrap().into())
        .await
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.material, created.material);
    let reused = client
        .upload_material(
            "task-1",
            bundle(Some("m-1"), &[("a.txt", b"two")])
                .encode()
                .unwrap()
                .into(),
        )
        .await;
    assert_eq!(refused_code(reused), "material_id_reused");

    for n in 2..=4 {
        client
            .upload_material(
                "task-1",
                bundle(None, &[("f", format!("{n}").as_bytes())])
                    .encode()
                    .unwrap()
                    .into(),
            )
            .await
            .unwrap();
    }
    let before = fixture.stored_files();
    let over = client
        .upload_material(
            "task-1",
            bundle(None, &[("f", b"5")]).encode().unwrap().into(),
        )
        .await;
    assert_eq!(refused_code(over), "material_quota_exceeded");
    assert_eq!(fixture.stored_files(), before, "nothing was added");
}

#[tokio::test]
async fn another_owner_can_neither_submit_read_download_nor_delete() {
    let fixture = Fixture::new().await;
    let (spec, _) = git_spec(&fixture.root);
    fixture.create("task-1", spec);
    let upload = fixture
        .client()
        .upload_material(
            "task-1",
            bundle(Some("mine"), &[("a.txt", SECRET_CONTENT.as_bytes())])
                .encode()
                .unwrap()
                .into(),
        )
        .await
        .unwrap();
    let before = fixture.stored_files();

    // The owner gate puts the caller's identity on every request; another
    // identity is indistinguishable from a task that does not exist.
    let intruder = OwnerRef::new("https://issuer.example\nsomeone-else");
    let submitted = fixture
        .service
        .upload_material(
            &intruder,
            "task-1",
            in_process(
                bundle(Some("theirs"), &[("b.txt", b"b")])
                    .encode()
                    .unwrap()
                    .into(),
            ),
        )
        .await;
    assert!(matches!(
        submitted,
        Err(narsor_engine::controller_protocol::ProtocolError::NotFound)
    ));
    assert!(matches!(
        fixture.service.list_materials(&intruder, "task-1"),
        Err(narsor_engine::controller_protocol::ProtocolError::NotFound)
    ));
    assert!(matches!(
        fixture.service.material(&intruder, "task-1", "mine"),
        Err(narsor_engine::controller_protocol::ProtocolError::NotFound)
    ));
    let sha256 = sha256_hex(SECRET_CONTENT.as_bytes());
    assert!(matches!(
        fixture
            .service
            .material_blob(&intruder, "task-1", "mine", &sha256),
        Err(narsor_engine::controller_protocol::ProtocolError::NotFound)
    ));
    assert!(matches!(
        fixture.service.delete_material(&intruder, "task-1", "mine"),
        Err(narsor_engine::controller_protocol::ProtocolError::NotFound)
    ));
    assert_eq!(fixture.stored_files(), before, "nothing changed");

    // Only digests the manifest names are ever served.
    let stray = sha256_hex(b"something else");
    assert!(matches!(
        fixture
            .service
            .material_blob(&fixture.owner(), "task-1", "mine", &stray),
        Err(narsor_engine::controller_protocol::ProtocolError::NotFound)
    ));

    // The owner deletes it; afterwards nothing is served.
    let deleted = fixture
        .client()
        .delete_material("task-1", "mine")
        .await
        .expect("delete");
    assert_eq!(deleted.material_id, upload.material.material_id);
    assert!(matches!(
        fixture.client().fetch_material("task-1", "mine").await,
        Err(ClientError::NotFound)
    ));
    assert!(fixture.stored_files().is_empty());
    assert!(fixture.events("task-1").iter().any(|event| matches!(
        event.kind,
        TaskEventKind::MaterialChanged {
            change: MaterialChange::Deleted,
            ..
        }
    )));
}

#[tokio::test]
async fn materials_expire_and_the_stream_carries_counts_only() {
    let fixture = Fixture::new().await;
    let (spec, _) = git_spec(&fixture.root);
    fixture.create("task-1", spec);
    let client = fixture.client();

    // Follow the task's live event stream while a material comes and goes.
    let stream = reqwest::Client::new()
        .get(format!(
            "{}/control/v1/events/stream?task_id=task-1",
            fixture.url()
        ))
        .bearer_auth(fixture.handle().token())
        .send()
        .await
        .expect("open the stream");
    assert!(stream.status().is_success());

    let mut short = bundle(None, &[(SECRET_NAME, SECRET_CONTENT.as_bytes())]);
    short.manifest.ttl_secs = Some(1);
    let uploaded = client
        .upload_material("task-1", short.encode().unwrap().into())
        .await
        .unwrap();
    assert!(
        uploaded.material.expires_at <= uploaded.material.created_at + chrono::Duration::seconds(1)
    );
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert!(client
        .materials("task-1")
        .await
        .unwrap()
        .materials
        .is_empty());
    assert!(fixture.stored_files().is_empty());
    assert!(fixture.events("task-1").iter().any(|event| matches!(
        event.kind,
        TaskEventKind::MaterialChanged {
            change: MaterialChange::Expired,
            files: 1,
            ..
        }
    )));

    // Read what the stream delivered meanwhile.
    let mut stream = stream;
    let mut text = String::new();
    let deadline = Instant::now() + Duration::from_secs(3);
    while !text.contains("expired") && Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), stream.chunk()).await {
            Ok(Ok(Some(chunk))) => text.push_str(&String::from_utf8_lossy(&chunk)),
            Ok(_) => break,
            Err(_) => {}
        }
    }
    assert!(text.contains("material_changed"), "{text}");
    assert!(
        text.contains("uploaded") && text.contains("expired"),
        "{text}"
    );
    assert!(!text.contains(SECRET_CONTENT));
    assert!(!text.contains(SECRET_NAME));
}

#[tokio::test]
async fn recovery_drops_interrupted_uploads_and_an_archived_tasks_leftovers() {
    let fixture = Fixture::new().await;
    let (spec, _) = git_spec(&fixture.root);
    fixture.create("task-1", spec.clone());
    fixture.create("task-2", spec);
    for task in ["task-1", "task-2"] {
        fixture
            .client()
            .upload_material(
                task,
                bundle(Some("kept"), &[("a.txt", b"a")])
                    .encode()
                    .unwrap()
                    .into(),
            )
            .await
            .unwrap();
    }
    // A crash in the middle of an upload, and an archive whose purge never
    // ran: task-2's materials are put back after the archive removed them.
    let interrupted = fixture.state.join("materials/.staging/0123/blobs");
    fs::create_dir_all(&interrupted).unwrap();
    fs::write(interrupted.join(sha256_hex(b"half")), "ha").unwrap();
    let saved = fixture.root.dir("saved");
    let task_2 = fixture.state.join("materials/task-2");
    fs::rename(&task_2, saved.join("task-2")).unwrap();
    fixture.command("task-2", OwnerCommand::Archive);
    fs::rename(saved.join("task-2"), &task_2).unwrap();

    let recovery = fixture.service.control_plane().recover().expect("recover");
    assert_eq!(recovery.removed_material_leftovers.len(), 2);
    assert!(!fixture.state.join("materials/.staging/0123").exists());
    assert!(!task_2.exists());
    assert_eq!(
        fixture
            .service
            .list_materials(&fixture.owner(), "task-1")
            .unwrap()
            .materials
            .len(),
        1,
        "a live task's material survives recovery"
    );
}

// ---------------------------------------------------------------------------
// Download path safety
// ---------------------------------------------------------------------------

#[test]
fn downloads_write_only_inside_the_folder_and_never_through_links() {
    let root = TempRoot::new();
    let material = bundle(None, &[("docs/a.md", b"new a"), ("b.txt", b"new b")]);
    let files = material.manifest.files.clone();
    let blobs: BTreeMap<String, Bytes> = files
        .iter()
        .map(|file| {
            let content: &[u8] = if file.path == "b.txt" {
                b"new b"
            } else {
                b"new a"
            };
            (file.sha256.clone(), Bytes::copy_from_slice(content))
        })
        .collect();

    // An existing file is refused without --force, and nothing is written.
    let dest = root.dir("dest");
    fs::write(dest.join("b.txt"), "mine").unwrap();
    assert!(write_files(&dest, &files, &blobs, WriteMode::Refuse).is_err());
    assert!(!dest.join("docs").exists(), "a refusal writes nothing");
    assert_eq!(fs::read_to_string(dest.join("b.txt")).unwrap(), "mine");
    write_files(&dest, &files, &blobs, WriteMode::Overwrite).expect("--force");
    assert_eq!(fs::read_to_string(dest.join("b.txt")).unwrap(), "new b");
    assert_eq!(fs::read_to_string(dest.join("docs/a.md")).unwrap(), "new a");

    // A link on the way out of the folder is refused, even with --force.
    let elsewhere = root.dir("elsewhere");
    let linked = root.dir("linked");
    std::os::unix::fs::symlink(&elsewhere, linked.join("docs")).unwrap();
    assert!(write_files(&linked, &files, &blobs, WriteMode::Overwrite).is_err());
    assert!(files_below(&elsewhere).is_empty(), "nothing escaped");
    assert!(!linked.join("b.txt").exists());

    // A link at the target itself is not written through.
    let target_link = root.dir("target-link");
    let victim = elsewhere.join("victim");
    fs::write(&victim, "untouched").unwrap();
    std::os::unix::fs::symlink(&victim, target_link.join("b.txt")).unwrap();
    assert!(write_files(&target_link, &files, &blobs, WriteMode::Overwrite).is_err());
    assert_eq!(fs::read_to_string(&victim).unwrap(), "untouched");

    // A manifest that escapes is refused by the writer too.
    let escaping = vec![MaterialFile {
        path: "../escape".to_string(),
        size: 5,
        sha256: sha256_hex(b"new b"),
    }];
    let fresh = root.dir("fresh");
    assert!(write_files(&fresh, &escaping, &blobs, WriteMode::Overwrite).is_err());
    assert!(!root.path.join("escape").exists());

    // Content that does not match the manifest is never written.
    let lying: BTreeMap<String, Bytes> = files
        .iter()
        .map(|file| (file.sha256.clone(), Bytes::from_static(b"other")))
        .collect();
    let honest = root.dir("honest");
    assert!(write_files(&honest, &files, &lying, WriteMode::Refuse).is_err());
    assert!(files_below(&honest).is_empty());
}

#[test]
fn a_snapshot_refuses_an_explicitly_named_link_and_oversize_folders() {
    let root = TempRoot::new();
    let target = root.dir("target");
    fs::write(target.join("f"), "x").unwrap();
    let link = root.path.join("link");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let limits = test_limits();
    assert!(snapshot_paths(&[link], &limits).is_err());

    let big = root.dir("big");
    for n in 0..3 {
        fs::write(big.join(format!("{n}")), vec![b'x'; 4000]).unwrap();
    }
    assert!(snapshot_paths(&[big], &limits).is_err());
}
