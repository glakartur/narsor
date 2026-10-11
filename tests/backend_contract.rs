//! The backend contract suite (U4) run against the in-memory fake adapter
//! and the local adapter — the same scenarios, unchanged, for both (AE2).
//!
//! The suite itself lives in `backend_contract_support` so other adapters'
//! test crates (U5's Kubernetes adapter against a mock API) expand it too.

mod environment_support;
#[macro_use]
mod backend_contract_support;

use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::time::Duration;

use backend_contract_support::ContractTarget;
use narsor::backends::{
    ExecutionBackend, FakeBackend, LocalBackend, LocalBackendConfig, TaskResources,
};
use narsor::domain::{ResolvedEnvironment, ResourceProfile, TaskId, TaskLaunchSpec, TaskWorkspace};

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
        TaskLaunchSpec::new(
            "wf",
            1,
            serde_json::json!({}),
            TaskWorkspace::None,
            ResolvedEnvironment::harness_only("claude-code"),
            ResourceProfile::new(1000, 2048),
        )
    }

    fn kill_workload(&self, backend: &FakeBackend, task: &TaskResources) {
        assert!(backend.kill_workload(task), "a workload to kill");
    }

    fn patience(&self) -> Duration {
        Duration::from_secs(15)
    }
}

contract_suite!(fake, crate::FakeTarget);

// ---------------------------------------------------------------------------
// The local adapter
// ---------------------------------------------------------------------------

/// `narsor runner` processes over local directories.
struct LocalTarget {
    config: LocalBackendConfig,
}

/// A workflow of one script step: it finishes at once, and the runner then
/// idles until the control plane tells it otherwise.
const WORKFLOW: &str = r#"
name: contract-task
version: 1
states:
  work:
    action:
      type: script
      command: sh
      args: [-c, "echo worked >> \"$HOME/steps\"; printf '{\"output\": \"done\"}'"]
    outputs: [done]
    transitions:
      done: completed
  completed:
    description: Done.
terminal_states: [completed]
"#;

impl ContractTarget for LocalTarget {
    type Backend = LocalBackend;

    fn setup(scratch: &Path) -> Option<Self> {
        // SAFETY: `geteuid` has no preconditions.
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("skipped: the runner refuses to run as root");
            return None;
        }
        let mut config =
            LocalBackendConfig::with_program(scratch.join("local"), env!("CARGO_BIN_EXE_narsor"));
        config.runner_args = [
            "--poll-wait-secs",
            "1",
            "--retry-min-ms",
            "50",
            "--retry-max-ms",
            "300",
            "--report-interval-ms",
            "50",
            "--problem-report-secs",
            "10",
        ]
        .into_iter()
        .map(OsString::from)
        .collect();
        config.env = vec![("RUST_LOG".into(), "narsor=info".into())];
        Some(Self { config })
    }

    fn backend(&self) -> LocalBackend {
        // A new instance knows no children: exactly a restarted controller.
        LocalBackend::new(self.config.clone())
    }

    fn launch_spec(&self) -> TaskLaunchSpec {
        let definition = narsor::workflow::load_from_str(WORKFLOW).expect("a valid workflow");
        TaskLaunchSpec::new(
            definition.id.clone(),
            definition.version,
            serde_json::json!({}),
            TaskWorkspace::None,
            ResolvedEnvironment::harness_only("claude-code"),
            ResourceProfile::new(1000, 2048),
        )
        .with_definition(&definition)
        .expect("snapshot the definition")
    }

    fn kill_workload(&self, backend: &LocalBackend, task: &TaskResources) {
        let pid = backend.runner_pid(task).expect("a running runner to kill");
        // SAFETY: signalling the runner's own process group.
        unsafe {
            libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
        }
    }

    fn cleanup(&self, backend: &LocalBackend) {
        // Runners outlive their controller by design; a finished scenario
        // must not leave any behind.
        let Ok(entries) = fs::read_dir(self.config.root.join("workloads")) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let Ok(task_id) = TaskId::parse(&name) else {
                continue;
            };
            let Ok(task) = backend.resources(&task_id, &name) else {
                continue;
            };
            if let Some(pid) = backend.runner_pid(&task) {
                // SAFETY: as above.
                unsafe {
                    libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
                }
            }
        }
    }
}

contract_suite!(local, crate::LocalTarget);

// ---------------------------------------------------------------------------
// Local adapter specifics
// ---------------------------------------------------------------------------

/// The local adapter runs the very same Engine through the runner: its
/// events live on the task storage, survive the loss of the process and an
/// owner resume, and a finished step is never executed again (R13).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn local_engine_work_survives_loss_and_owner_resume() {
    use narsor::orchestration::{OwnerCommand, TaskLifecycle};

    let Some(mut h) = backend_contract_support::Harness::<LocalTarget>::without_auto_resume().await
    else {
        return;
    };
    h.start_controller();
    let before = finished_work(&h).await;

    h.kill_workload("task-1");
    h.wait_lifecycle("task-1", TaskLifecycle::Interrupted).await;
    h.must("task-1", OwnerCommand::Resume, "resume-1");
    h.wait_lifecycle("task-1", TaskLifecycle::Running).await;

    assert_work_continued(&h, &before);
}

/// The same, with nobody deciding: the resume policy launches a new runner
/// on the storage, which continues the work without repeating it, and the
/// task's history shows the loss and the automatic resume (R1, R2, AE2).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn local_engine_work_survives_loss_and_automatic_resume() {
    use narsor::orchestration::{Observation, TaskLifecycle};

    let Some(mut h) = backend_contract_support::Harness::<LocalTarget>::new().await else {
        return;
    };
    h.start_controller();
    let before = finished_work(&h).await;
    let first = h.record("task-1").runner.unwrap().incarnation;

    h.kill_workload("task-1");
    h.wait_for("a new runner", |h| {
        let record = h.record("task-1");
        record.lifecycle == TaskLifecycle::Running
            && record
                .runner
                .is_some_and(|runner| runner.incarnation != first)
    })
    .await;

    assert!(h
        .reported("task-1")
        .contains(&Observation::WorkloadLost { state_known: true }));
    assert_eq!(
        backend_contract_support::auto_resumes(&h.events("task-1")),
        vec![(1, 5)]
    );
    assert_eq!(h.counters().workloads_created, 2);
    assert_work_continued(&h, &before);
}

/// What the finished workflow left on the task's storage before a loss.
struct FinishedWork {
    storage: std::path::PathBuf,
    execution_id: String,
    events: Vec<narsor::domain::Event>,
    cursor: u64,
}

/// Runs task-1 until its workflow finished and records what it left.
async fn finished_work(h: &backend_contract_support::Harness<LocalTarget>) -> FinishedWork {
    use narsor::domain::ExecutionStatus;
    use narsor::persistence::ExecutionStore;

    h.running("task-1").await;
    h.wait_for("the workflow to finish", |h| {
        h.record("task-1")
            .runner
            .is_some_and(|runner| runner.snapshot.execution_status == ExecutionStatus::Completed)
    })
    .await;
    let storage = h.probe.inner().storage_dir(&h.resources("task-1"));
    let store = ExecutionStore::new(storage.join("state"));
    let execution_id = store.list_execution_ids().unwrap().remove(0);
    FinishedWork {
        events: store.event_log(&execution_id).read_all().unwrap(),
        cursor: h.record("task-1").runner.unwrap().snapshot.event_cursor,
        storage,
        execution_id,
    }
}

/// The resumed runner continued `before` on the same storage: the same
/// execution, its log only appended to, the finished step not run again.
fn assert_work_continued(
    h: &backend_contract_support::Harness<LocalTarget>,
    before: &FinishedWork,
) {
    use narsor::domain::ExecutionStatus;
    use narsor::persistence::ExecutionStore;

    let record = h.record("task-1");
    let runner = record.runner.unwrap();
    assert_eq!(runner.launch_generation, record.generation);
    assert!(runner.snapshot.event_cursor >= before.cursor);
    let store = ExecutionStore::new(before.storage.join("state"));
    assert_eq!(
        store.list_execution_ids().unwrap(),
        std::slice::from_ref(&before.execution_id),
        "the same execution, on the same storage"
    );
    let events_after = store.event_log(&before.execution_id).read_all().unwrap();
    let sequences = |events: &[narsor::domain::Event]| -> Vec<(u64, String)> {
        events
            .iter()
            .map(|event| (event.sequence, format!("{:?}", event.data)))
            .collect()
    };
    assert!(events_after.len() >= before.events.len());
    assert_eq!(
        sequences(&events_after[..before.events.len()]),
        sequences(&before.events),
        "the event log is continued, never rewritten"
    );
    assert_eq!(
        store.load_execution(&before.execution_id).unwrap().status,
        ExecutionStatus::Completed
    );
    assert_eq!(
        fs::read_to_string(before.storage.join("home/steps")).unwrap(),
        "worked\n",
        "the finished step ran once"
    );
}

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
    use narsor::backends::{RunnerCredential, WorkloadLaunch};

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
        launch_spec: FakeTarget {
            platform: FakeBackend::new(),
        }
        .launch_spec(),
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
