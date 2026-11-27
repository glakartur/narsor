//! The automation catalogue as the control plane reads it through the
//! launch resolver: `always` queues are checked against the real workflow
//! catalogue, and a script is read at the time of the call.

use std::fs;
use std::path::{Path, PathBuf};

use crate::environment_support::{resolver, seed_catalog};
use narsor_engine::automations::HookPhase;
use narsor_engine::environment::LaunchResolver;

/// A seeded catalogue root that deletes itself.
struct TempRoot(PathBuf);

impl TempRoot {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("narsor-automations-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).expect("create temp root");
        seed_catalog(&path);
        Self(path)
    }

    fn write(&self, relative: &str, text: &str) {
        let path = self.0.join(relative);
        fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        fs::write(path, text).expect("write");
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn an_always_queue_loads_and_one_with_the_removed_tasks_is_a_problem() {
    let root = TempRoot::new();
    root.write(
        "automations/jira-new-issues/automation.yaml",
        "description: New Jira issues become tasks\n",
    );
    root.write(
        "automations/jira-new-issues/check.sh",
        "#!/bin/sh\nexit 0\n",
    );
    let queue = "automations: [jira-new-issues]\nmode: always\nschedule: {interval_secs: 600}\n";
    root.write("queues/jira-intake.yaml", queue);
    root.write(
        "queues/old-intake.yaml",
        &format!("{queue}tasks: {{workflow_id: wf}}\n"),
    );
    let resolver = resolver(root.path());

    let load = resolver.automation_catalog();

    assert!(load.queues.contains_key("jira-intake"));
    assert!(!load.queues.contains_key("old-intake"));
    assert_eq!(load.problems.len(), 1, "{:?}", load.problems);
    assert_eq!(load.problems[0].file, Path::new("queues/old-intake.yaml"));
    assert!(load.problems[0].reason.contains("'tasks' is removed"));
    let script = resolver
        .automation_script("jira-new-issues", HookPhase::Check)
        .expect("check script");
    assert_eq!(script.text, "#!/bin/sh\nexit 0\n");
}
