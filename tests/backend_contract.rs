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
    use narsor::domain::ExecutionStatus;
    use narsor::orchestration::{OwnerCommand, TaskLifecycle};
    use narsor::persistence::ExecutionStore;

    let Some(mut h) = backend_contract_support::Harness::<LocalTarget>::new().await else {
        return;
    };
    h.start_controller();
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
    let events_before = store.event_log(&execution_id).read_all().unwrap();
    let cursor_before = h.record("task-1").runner.unwrap().snapshot.event_cursor;

    h.kill_workload("task-1");
    h.wait_lifecycle("task-1", TaskLifecycle::Interrupted).await;
    h.must("task-1", OwnerCommand::Resume, "resume-1");
    h.wait_lifecycle("task-1", TaskLifecycle::Running).await;

    let record = h.record("task-1");
    let runner = record.runner.unwrap();
    assert_eq!(runner.launch_generation, record.generation);
    assert!(runner.snapshot.event_cursor >= cursor_before);
    assert_eq!(
        store.list_execution_ids().unwrap(),
        std::slice::from_ref(&execution_id),
        "the same execution, on the same storage"
    );
    let events_after = store.event_log(&execution_id).read_all().unwrap();
    let sequences = |events: &[narsor::domain::Event]| -> Vec<(u64, String)> {
        events
            .iter()
            .map(|event| (event.sequence, format!("{:?}", event.data)))
            .collect()
    };
    assert!(events_after.len() >= events_before.len());
    assert_eq!(
        sequences(&events_after[..events_before.len()]),
        sequences(&events_before),
        "the event log is continued, never rewritten"
    );
    assert_eq!(
        store.load_execution(&execution_id).unwrap().status,
        ExecutionStatus::Completed
    );
    assert_eq!(
        fs::read_to_string(storage.join("home/steps")).unwrap(),
        "worked\n",
        "the finished step ran once"
    );
}
