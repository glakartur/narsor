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
