//! The backend contract suite (U4) run against the in-memory fake adapter,
//! the local adapter and the local adapter in the Docker work mode on an
//! in-memory Docker — the same scenarios, unchanged, for all (AE2). The
//! local adapter's runners are the suite's stub runner processes, not the
//! real `narsor-runner`; in the Docker work mode the fake Docker runs them
//! as its containers' processes.
//!
//! The suite itself lives in `backend_contract_support` so other adapters'
//! test crates (U5's Kubernetes adapter against a mock API) expand it too.

use crate::backend_contract_support;

use std::fs;
use std::path::Path;
use std::time::Duration;

use backend_contract_support::fake::FakeBackend;
use backend_contract_support::{stub_runner, ContractTarget};
use narsor_controller::local::backend::{LocalBackend, LocalBackendConfig};
use narsor_controller_runtime::backend::{ExecutionBackend, TaskResources};
use narsor_engine::domain::{
    ResolvedEnvironment, ResourceProfile, TaskId, TaskLaunchSpec, TaskWorkspace,
};

// ---------------------------------------------------------------------------
// The fake adapter
// ---------------------------------------------------------------------------

/// The in-memory platform with simulated runners.
struct FakeTarget {
    platform: FakeBackend,
}

impl ContractTarget for FakeTarget {
    type Backend = FakeBackend;

    fn setup(_scratch: &Path) -> Option<Self> {
        Some(Self {
            platform: FakeBackend::new(),
        })
    }

    fn backend(&self) -> FakeBackend {
        // A clone shares the platform: what a second controller would see.
        self.platform.clone()
    }

    fn launch_spec(&self) -> TaskLaunchSpec {
        contract_launch_spec()
    }

    fn kill_workload(&self, backend: &FakeBackend, task: &TaskResources) {
        assert!(backend.kill_workload(task), "a workload to kill");
    }

    fn kill_reader(&self, backend: &FakeBackend, task: &TaskResources) -> bool {
        backend.kill_reader(task)
    }

    fn patience(&self) -> Duration {
        Duration::from_secs(15)
    }
}

contract_suite!(fake, crate::backend_contract::FakeTarget);

/// R4: a workload the platform loses wakes the controller at once — the
/// loss is reported long before the periodic observation would notice it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lost_workload_is_reported_at_the_platform_event_not_at_the_idle_pace() {
    use backend_contract_support::Harness;
    use narsor_engine::orchestration::RunnerState;

    let mut h = Harness::<FakeTarget>::without_auto_resume()
        .await
        .expect("the fake runs anywhere");
    // Without an event, the next observation would come in 40 s at the
    // earliest: the lease is renewed every 40 s, and nothing else wakes it.
    h.start_controller_with(|config| {
        config.poll_wait = Duration::from_secs(120);
        config.lease_ttl = Duration::from_secs(120);
    });
    h.running("task-1").await;

    let killed = std::time::Instant::now();
    h.kill_workload("task-1");
    h.wait_runner_state("task-1", RunnerState::Interrupted)
        .await;

    assert!(
        killed.elapsed() < Duration::from_secs(5),
        "reported after {:?}",
        killed.elapsed()
    );
}

/// R2: the launch spec is served to the lease holder only, and only for the
/// task's current generation; a command itself carries none.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_launch_spec_is_served_to_the_lease_holder_for_the_current_generation_only() {
    use backend_contract_support::Harness;
    use narsor_engine::controller_protocol::ControllerClient;
    use narsor_shared::api_error::ClientError;

    let h = Harness::<FakeTarget>::new()
        .await
        .expect("the fake runs anywhere");
    let client = ControllerClient::new(&h.url, h.enroll());
    h.create("task-1");
    let generation = h.record("task-1").generation;
    let code = |error: ClientError| match error {
        ClientError::Refused { code, .. } => code,
        other => panic!("expected a refusal, got {other:?}"),
    };

    let without_lease = client.fetch_launch_spec("task-1", generation).await;
    assert_eq!(code(without_lease.unwrap_err()), "lease_not_held");
    client.acquire_lease(Some(30)).await.expect("lease");
    let stale = client.fetch_launch_spec("task-1", generation + 1).await;
    assert_eq!(code(stale.unwrap_err()), "stale_generation");
    let spec = client
        .fetch_launch_spec("task-1", generation)
        .await
        .expect("the spec");

    assert_eq!(spec, serde_json::to_value(contract_launch_spec()).unwrap());
}

/// R5: a working runner fetches its task's launch spec with its own
/// credential; a reader never gets it, nor does the runner of a launch a
/// later resume superseded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_launch_spec_is_served_to_the_current_working_runner_only() {
    use backend_contract_support::Harness;
    use narsor_engine::controller_protocol::RunnerClient;
    use narsor_engine::orchestration::{OwnerCommand, RunnerState};
    use narsor_shared::api_error::ClientError;

    let Some(mut h) = Harness::<FakeTarget>::new().await else {
        return;
    };
    h.start_controller();
    h.running("task-1").await;
    let task = h.resources("task-1");
    let runner = |reader: bool| {
        let credential = h
            .target
            .platform
            .credential_of(&task, reader)
            .expect("a launched runner");
        RunnerClient::new(&h.url, credential)
    };
    let worker = runner(false);

    let spec = worker.launch_spec().await.expect("the worker's spec");
    assert_eq!(spec, serde_json::to_value(contract_launch_spec()).unwrap());

    h.must("task-1", OwnerCommand::Pause, "pause-1");
    h.wait_runner_state("task-1", RunnerState::Stopped).await;
    h.lease_reader("task-1", 60);
    h.running_reader("task-1").await;
    let refused = runner(true)
        .launch_spec()
        .await
        .expect_err("a reader is refused");
    assert!(matches!(refused, ClientError::Forbidden), "{refused:?}");

    h.must("task-1", OwnerCommand::Resume, "resume-1");
    let superseded = worker
        .launch_spec()
        .await
        .expect_err("an older launch is refused");
    assert!(
        matches!(superseded, ClientError::Unauthorized),
        "{superseded:?}"
    );
}

// ---------------------------------------------------------------------------
// Incremental command reads (U3), with a controller driven by hand
// ---------------------------------------------------------------------------

mod incremental_reads {
    use super::FakeTarget;
    use crate::backend_contract_support::Harness;
    use narsor_engine::controller_protocol::{
        CommandsResponse, ControllerClient, DesiredAction, DesiredCommand, ObservedSummary,
        ReportRequest,
    };
    use narsor_engine::domain::TaskId;
    use narsor_engine::orchestration::{Observation, OwnerCommand, RunnerSnapshot, RunnerState};
    use std::time::{Duration, Instant};

    /// A controller identity holding the lease, reading and reporting by
    /// hand.
    struct Reader {
        client: ControllerClient,
        epoch: u64,
        cursor: Option<u64>,
    }

    impl Reader {
        async fn new(h: &Harness<FakeTarget>) -> Self {
            let client = ControllerClient::new(&h.url, h.enroll());
            let epoch = client.acquire_lease(Some(60)).await.expect("lease").epoch;
            Self {
                client,
                epoch,
                cursor: None,
            }
        }

        /// A read since the last one, waiting up to `wait_secs`.
        async fn read(&mut self, wait_secs: u64) -> CommandsResponse {
            let response = self
                .client
                .commands(wait_secs, self.cursor)
                .await
                .expect("commands");
            self.cursor = Some(response.cursor);
            response
        }

        /// The one command of an incremental read, which must be `task`'s.
        async fn changed(&mut self, task: &str) -> DesiredCommand {
            let response = self.read(0).await;
            assert!(!response.full, "{response:?}");
            match <[DesiredCommand; 1]>::try_from(response.commands) {
                Ok([command]) if command.task_id.as_str() == task => command,
                Ok(other) => panic!("expected one command of {task}, got {other:?}"),
                Err(commands) => panic!("expected one command of {task}, got {commands:?}"),
            }
        }

        async fn nothing_changed(&mut self) {
            let response = self.read(0).await;
            assert!(
                !response.full && response.commands.is_empty(),
                "{response:?}"
            );
        }

        async fn report(&self, h: &Harness<FakeTarget>, task: &str, observation: Observation) {
            self.client
                .report(&ReportRequest {
                    task_id: task.to_string(),
                    lease_epoch: self.epoch,
                    desired_generation: h.record(task).generation,
                    observation,
                    resource: None,
                    summary: ObservedSummary::default(),
                    observed_at: chrono::Utc::now(),
                })
                .await
                .expect("report");
        }
    }

    /// A runner snapshot of `task`'s first launch, as its runner sends one.
    fn runner_reports(
        h: &Harness<FakeTarget>,
        task: &str,
        sequence: u64,
        event_cursor: u64,
        safe_point: bool,
    ) {
        let snapshot: RunnerSnapshot = serde_json::from_value(serde_json::json!({
            "snapshot_sequence": sequence,
            "execution_status": "running",
            "event_cursor": event_cursor,
            "safe_point": safe_point,
            "observed_at": chrono::Utc::now(),
        }))
        .expect("a snapshot");
        let task_id = TaskId::parse(task).unwrap();
        h.service
            .control_plane()
            .record_runner_snapshot(&task_id, "inc-1", 1, snapshot)
            .expect("record the snapshot");
        h.service.notify(&task_id);
    }

    /// R3, KTD1: after a full read, an incremental read carries exactly the
    /// tasks whose desired state changed — also when no generation moved:
    /// the start accepted, runner progress, the runner ready, a safe point,
    /// an admission, a reader lease — and nothing on an idle backend or for
    /// a progress-only snapshot. An archived task is forgotten once.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn every_change_a_controller_acts_on_reaches_the_next_incremental_read() {
        let h = Harness::<FakeTarget>::without_auto_resume()
            .await
            .expect("the fake runs anywhere");
        h.set_limit(1);
        h.create("task-1");
        h.create("task-2");
        let mut c = Reader::new(&h).await;

        let full = c.read(0).await;
        assert!(full.full);
        let names: Vec<_> = full.commands.iter().map(|c| c.task_id.as_str()).collect();
        assert_eq!(names, ["task-1"], "task-2 waits for a slot");
        let generation = full.commands[0].generation;
        c.nothing_changed().await;

        c.report(&h, "task-1", Observation::StartAccepted).await;
        let provisioning = c.changed("task-1").await;
        assert_eq!(provisioning.action, DesiredAction::Provision);
        runner_reports(&h, "task-1", 1, 1, false);
        let progress = c.changed("task-1").await;
        assert_eq!(progress.runner.expect("progress").incarnation, "inc-1");
        c.report(&h, "task-1", Observation::RunnerReady).await;
        let running = c.changed("task-1").await;
        assert_eq!(running.action, DesiredAction::Run);
        assert_eq!(running.generation, generation, "no generation moved");

        // A snapshot that only moves the event cursor changes nothing.
        runner_reports(&h, "task-1", 2, 5, false);
        c.nothing_changed().await;

        h.must("task-1", OwnerCommand::Pause, "pause-1");
        let pausing = c.changed("task-1").await;
        assert_eq!(pausing.action, DesiredAction::PauseAtSafePoint);
        runner_reports(&h, "task-1", 3, 6, true);
        let acknowledged = c.changed("task-1").await;
        assert!(
            acknowledged
                .runner
                .expect("progress")
                .safe_point_acknowledged
        );
        c.report(&h, "task-1", Observation::PausedAtSafePoint).await;
        let response = c.read(0).await;
        let actions: Vec<_> = response
            .commands
            .iter()
            .map(|c| (c.task_id.as_str(), c.action))
            .collect();
        assert_eq!(
            actions,
            [
                ("task-1", DesiredAction::KeepPaused),
                ("task-2", DesiredAction::Start)
            ],
            "the freed slot admits task-2"
        );

        h.lease_reader("task-1", 60);
        assert!(c.changed("task-1").await.reader_needed);

        // Room for both, so the archive's slot does not take task-2's back.
        h.set_limit(2);

        h.archive_in_control_plane("task-1", "archive-1");
        assert_eq!(c.changed("task-1").await.action, DesiredAction::Archive);
        c.report(&h, "task-1", Observation::ResourcesDeleted).await;
        assert_eq!(c.changed("task-1").await.action, DesiredAction::Forget);
        c.nothing_changed().await;
    }

    /// KTD1: a cursor the control plane does not know — one from before it
    /// restarted, or from under another lease epoch — is answered in full.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unknown_cursor_or_a_new_lease_epoch_is_answered_in_full() {
        let h = Harness::<FakeTarget>::without_auto_resume()
            .await
            .expect("the fake runs anywhere");
        h.create("task-1");
        let mut c = Reader::new(&h).await;
        let cursor = c.read(0).await.cursor;

        // A control plane that restarted remembers no delivery.
        c.cursor = Some(cursor.wrapping_add(1_000_000));
        let restarted = c.read(0).await;
        assert!(
            restarted.full && restarted.commands.len() == 1,
            "{restarted:?}"
        );

        c.client.release_lease(c.epoch).await.expect("release");
        c.epoch = c.client.acquire_lease(Some(60)).await.expect("lease").epoch;
        let reacquired = c.read(0).await;
        assert!(
            reacquired.full && reacquired.commands.len() == 1,
            "{reacquired:?}"
        );
        assert_eq!(reacquired.lease_epoch, c.epoch);
    }

    /// KTD1: heartbeats and progress-only snapshots do not end a long-poll;
    /// a reader lease running out does, with no mutation, at its expiry.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_long_poll_sleeps_through_progress_and_wakes_at_a_reader_lease_expiry() {
        let h = Harness::<FakeTarget>::without_auto_resume()
            .await
            .expect("the fake runs anywhere");
        h.create("task-1");
        h.create("task-2");
        let mut c = Reader::new(&h).await;
        c.read(0).await;
        // task-2 is lost and waits for its owner, who may read it.
        c.report(&h, "task-2", Observation::StartAccepted).await;
        c.report(
            &h,
            "task-2",
            Observation::WorkloadLost { state_known: true },
        )
        .await;
        c.report(&h, "task-1", Observation::StartAccepted).await;
        runner_reports(&h, "task-1", 1, 1, false);
        c.read(0).await;

        let polled = Instant::now();
        let client = &c.client;
        let (response, ()) = tokio::join!(client.commands(1, c.cursor), async {
            for sequence in 2..5 {
                tokio::time::sleep(Duration::from_millis(100)).await;
                client.heartbeat().await.expect("heartbeat");
                runner_reports(&h, "task-1", sequence, sequence * 10, false);
            }
        });
        let response = response.expect("commands");
        c.cursor = Some(response.cursor);
        assert!(response.commands.is_empty(), "{response:?}");
        assert!(polled.elapsed() >= Duration::from_millis(900), "it slept");

        h.lease_reader("task-2", 1);
        assert!(c.changed("task-2").await.reader_needed);
        let polled = Instant::now();
        let response = c.read(20).await;
        let expired = polled.elapsed();

        let [command] = <[DesiredCommand; 1]>::try_from(response.commands).expect("one");
        assert_eq!(command.task_id.as_str(), "task-2");
        assert!(!command.reader_needed);
        assert!(
            expired < Duration::from_secs(5),
            "answered at the expiry, not the wait: {expired:?}"
        );
    }

    /// D58: the resume policy's commands reach the controller once each, in
    /// the order they were issued.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn policy_resumes_are_delivered_once_and_in_order() {
        let h = Harness::<FakeTarget>::without_auto_resume()
            .await
            .expect("the fake runs anywhere");
        h.create("task-1");
        let mut c = Reader::new(&h).await;
        c.read(0).await;
        let mut delivered = Vec::new();
        for attempt in 0..2 {
            c.report(&h, "task-1", Observation::StartAccepted).await;
            c.report(
                &h,
                "task-1",
                Observation::WorkloadLost { state_known: true },
            )
            .await;
            assert_eq!(h.runner_state("task-1"), RunnerState::Interrupted);
            assert_eq!(c.changed("task-1").await.action, DesiredAction::Hold);
            // The second loss waits out its backoff.
            tokio::time::sleep(Duration::from_millis(150 * attempt)).await;
            h.service.auto_resume_pass();
            h.service.auto_resume_pass();
            let resumed = c.changed("task-1").await;
            delivered.push((resumed.generation, resumed.action));
            c.nothing_changed().await;
        }

        let first = delivered[0].0;
        assert_eq!(
            delivered,
            [
                (first, DesiredAction::Provision),
                (first + 1, DesiredAction::Provision)
            ],
            "{:?}",
            h.events("task-1")
        );
    }
}

// ---------------------------------------------------------------------------
// The local adapter
// ---------------------------------------------------------------------------

/// A launch spec of the contract's workflow, for either adapter.
fn contract_launch_spec() -> TaskLaunchSpec {
    let definition = backend_contract_support::workflow();
    TaskLaunchSpec::new(
        definition.id.clone(),
        definition.version,
        serde_json::json!({}),
        TaskWorkspace::none(),
        ResolvedEnvironment::harness_only("claude-code"),
        ResourceProfile::new(1000, 2048),
    )
    .with_definition(&definition)
    .expect("snapshot the definition")
}

/// Stub runner processes over local directories.
struct LocalTarget {
    config: LocalBackendConfig,
}

impl ContractTarget for LocalTarget {
    type Backend = LocalBackend;

    fn setup(scratch: &Path) -> Option<Self> {
        let program = stub_runner::install(&scratch.join("bin"));
        Some(Self {
            config: LocalBackendConfig::with_program(scratch.join("local"), program),
        })
    }

    fn backend(&self) -> LocalBackend {
        // A new instance knows no children: exactly a restarted controller.
        LocalBackend::new(self.config.clone())
    }

    fn launch_spec(&self) -> TaskLaunchSpec {
        contract_launch_spec()
    }

    fn kill_workload(&self, backend: &LocalBackend, task: &TaskResources) {
        let pid = backend.runner_pid(task).expect("a running runner to kill");
        // SAFETY: signalling the runner's own process group.
        unsafe {
            libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
        }
    }

    fn storage_dir(
        &self,
        backend: &LocalBackend,
        task: &TaskResources,
    ) -> Option<std::path::PathBuf> {
        Some(backend.storage_dir(task))
    }

    fn kill_reader(&self, backend: &LocalBackend, task: &TaskResources) -> bool {
        let Some(pid) = backend.reader_pid(task) else {
            return false;
        };
        // SAFETY: signalling the reader's own process group.
        unsafe {
            libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
        }
        true
    }

    fn cleanup(&self, backend: &LocalBackend) {
        // Runners outlive their controller by design; a finished scenario
        // must not leave any behind — workers and readers alike.
        for (directory, suffix) in [("workloads", ""), ("readers", "-reader")] {
            let Ok(entries) = fs::read_dir(self.config.root.join(directory)) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                let key = name.strip_suffix(suffix).unwrap_or(&name).to_string();
                let Ok(task_id) = TaskId::parse(&key) else {
                    continue;
                };
                let Ok(task) = backend.resources(&task_id, &key) else {
                    continue;
                };
                let pid = if suffix.is_empty() {
                    backend.runner_pid(&task)
                } else {
                    backend.reader_pid(&task)
                };
                if let Some(pid) = pid {
                    // SAFETY: as above.
                    unsafe {
                        libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
                    }
                }
            }
        }
    }
}

contract_suite!(
    local,
    crate::backend_contract::LocalTarget,
    ignore = "starts runner processes and kills their process groups; run with --include-ignored"
);

contract_suite!(
    local_docker,
    crate::backend_contract_support::local_docker::LocalDockerTarget,
    ignore = "starts runner processes and kills their process groups; run with --include-ignored"
);

/// D58 with containers: a container removed from outside (`docker rm -f`)
/// is observed as absent, and the automatic resume starts a new container
/// on the image the task last ran on, without a second build.
#[ignore = "starts runner processes and kills their process groups; run with --include-ignored"]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_container_removed_from_outside_is_resumed_on_the_same_image() {
    use backend_contract_support::local_docker::LocalDockerTarget;
    use backend_contract_support::Harness;
    use narsor_engine::orchestration::{Observation, RunnerState};

    let mut h = Harness::<LocalDockerTarget>::new()
        .await
        .expect("the fake Docker runs anywhere");
    h.start_controller();
    h.running("task-1").await;
    let first = h
        .target
        .container_of(&h.resources("task-1"))
        .expect("the task's container");

    narsor_controller::local::container::docker::DockerTransport::remove_container(
        &*h.target.docker,
        &first,
    )
    .expect("remove the container");
    h.wait_for("the replacement container", |h| {
        h.counters().workloads_created == 2 && h.runner_state("task-1") == RunnerState::Running
    })
    .await;

    assert!(
        h.reported("task-1")
            .contains(&Observation::WorkloadLost { state_known: true }),
        "the removed container is observed as lost"
    );
    let runs = h.target.docker.runs();
    assert_eq!(runs.len(), 2, "one container per start");
    assert_eq!(runs[0].name, first);
    assert_ne!(runs[1].name, first, "a new container");
    assert_eq!(runs[1].image, runs[0].image, "the same image id");
    assert_eq!(h.target.docker.builds().len(), 1, "no second build");
}

// ---------------------------------------------------------------------------
// Local adapter specifics
// ---------------------------------------------------------------------------

/// Whether `pid` names a live (not zombie) process.
fn process_running(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        stat.rfind(')')
            .and_then(|end| stat[end + 1..].split_whitespace().next())
            .is_some_and(|state| state != "Z")
    })
}

/// A runner that dies on its own leaves what it started in its process
/// group; removing its workload — what comes before every relaunch —
/// kills that orphan too, so it cannot write the storage under the next
/// runner.
#[tokio::test]
async fn removing_a_dead_runners_workload_kills_what_it_left_running() {
    use narsor_controller_runtime::backend::{RunnerCredential, WorkloadLaunch};

    let scratch = std::env::temp_dir().join(format!("narsor-orphan-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&scratch).unwrap();
    let child_pid_file = scratch.join("child.pid");
    let program = scratch.join("fake-runner");
    // Starts a long-lived child in its group, then waits for it.
    fs::write(
        &program,
        "#!/bin/sh\nsleep 600 &\necho $! > \"$CHILD_PID_FILE\"\nwait\n",
    )
    .unwrap();
    fs::set_permissions(
        &program,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    let mut config = LocalBackendConfig::with_program(scratch.join("local"), &program);
    config.env = vec![("CHILD_PID_FILE".into(), child_pid_file.clone().into())];
    let backend = LocalBackend::new(config);
    let task_id = TaskId::parse("task-1").unwrap();
    let task = backend.resources(&task_id, "task-1").unwrap();
    let launch = WorkloadLaunch {
        task_id: task_id.clone(),
        generation: 1,
        incarnation: "task-1-g1".into(),
        lease_epoch: 1,
        launch_spec: serde_json::to_value(contract_launch_spec()).unwrap(),
        control_plane_url: "http://127.0.0.1:9".into(),
        credential: RunnerCredential::new("nsr.test"),
    };
    backend.provision(&task, &launch).await.expect("provision");
    let leader = backend.runner_pid(&task).expect("a running runner");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let child: u32 = loop {
        if let Some(pid) = fs::read_to_string(&child_pid_file)
            .ok()
            .and_then(|text| text.trim().parse().ok())
        {
            break pid;
        }
        assert!(std::time::Instant::now() < deadline, "no child started");
        std::thread::sleep(Duration::from_millis(20));
    };

    // Only the runner dies; its child lives on in the group.
    // SAFETY: signalling the fake runner this test started.
    unsafe {
        libc::kill(leader as libc::pid_t, libc::SIGKILL);
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while backend.runner_pid(&task).is_some() {
        assert!(
            std::time::Instant::now() < deadline,
            "the runner never died"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(process_running(child), "the orphan still runs");

    let removal = backend.release_compute(&task).await.expect("removal");

    assert!(removal.is_complete(), "{removal:?}");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while process_running(child) {
        assert!(
            std::time::Instant::now() < deadline,
            "the orphan {child} survived the removal"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = fs::remove_dir_all(&scratch);
}
