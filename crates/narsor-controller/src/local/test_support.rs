//! What the tests of the local adapter share: a scratch backend over a
//! temporary root.

use std::fs;
use std::path::PathBuf;

use narsor_controller_runtime::backend::{ExecutionBackend, TaskResources, WorkloadLaunch};
use narsor_engine::domain::{TaskId, TaskLaunchSpec};

use super::backend::{LocalBackend, LocalBackendConfig};
use super::workload::Role;

/// A scratch backend whose "runner" is a script that records its
/// arguments and sleeps; stops everything it started when dropped.
pub(super) struct Scratch {
    pub(super) root: PathBuf,
    pub(super) backend: LocalBackend,
}

impl Scratch {
    pub(super) fn new() -> Self {
        let root = std::env::temp_dir().join(format!("narsor-local-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let program = root.join("fake-runner");
        fs::write(
            &program,
            "#!/bin/sh\necho \"$@\" >> \"$ARGS_FILE\"\nexec sleep 600\n",
        )
        .unwrap();
        fs::set_permissions(
            &program,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        let mut config = LocalBackendConfig::with_program(root.join("local"), &program);
        config.env = vec![("ARGS_FILE".into(), root.join("args").into())];
        Self {
            root,
            backend: LocalBackend::new(config),
        }
    }

    pub(super) fn task(&self) -> TaskResources {
        let id = TaskId::parse("task-1").unwrap();
        self.backend.resources(&id, "task-1").unwrap()
    }

    pub(super) fn launch(&self, incarnation: &str) -> WorkloadLaunch {
        use narsor_engine::domain::{ResolvedEnvironment, ResourceProfile, TaskWorkspace};
        let spec = TaskLaunchSpec::new(
            "wf",
            1,
            serde_json::json!({}),
            TaskWorkspace::none(),
            ResolvedEnvironment::harness_only("claude-code"),
            ResourceProfile::new(1000, 2048),
        );
        WorkloadLaunch {
            task_id: TaskId::parse("task-1").unwrap(),
            generation: 1,
            incarnation: incarnation.into(),
            lease_epoch: 1,
            launch_spec: serde_json::to_value(spec).unwrap(),
            control_plane_url: "http://127.0.0.1:9".into(),
            credential: narsor_controller_runtime::backend::RunnerCredential::new("nsr.test"),
        }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let task = self.task();
        for role in [Role::Work, Role::Read] {
            let _ = self.backend.remove_workload(&task, role);
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}
