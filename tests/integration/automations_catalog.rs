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

fn always_queue(workflow: &str) -> String {
    format!(
        "automations: [jira-new-issues]\nmode: always\nschedule: {{interval_secs: 600}}\n\
         tasks: {{workflow_id: {workflow}}}\n"
    )
}

#[test]
fn always_queue_takes_the_catalogue_workflow_and_one_naming_an_unknown_workflow_is_a_problem() {
    let root = TempRoot::new();
    root.write(
        "automations/jira-new-issues/automation.yaml",
        "description: New Jira issues become tasks\n",
    );
    root.write(
        "automations/jira-new-issues/check.sh",
        "#!/bin/sh\nexit 0\n",
    );
    root.write("queues/jira-intake.yaml", &always_queue("wf"));
    root.write("queues/lost-intake.yaml", &always_queue("no-such-workflow"));
    let resolver = resolver(root.path());

    let load = resolver.automation_catalog();

    let workflow = resolver.workflow_definition("wf").expect("seeded workflow");
    let tasks = load.queues["jira-intake"].tasks.as_ref().expect("tasks");
    assert_eq!(tasks.effect.workflow_version, workflow.version);
    assert!(!load.queues.contains_key("lost-intake"));
    assert_eq!(load.problems.len(), 1, "{:?}", load.problems);
    assert_eq!(load.problems[0].file, Path::new("queues/lost-intake.yaml"));
    let script = resolver
        .automation_script("jira-new-issues", HookPhase::Check)
        .expect("check script");
    assert_eq!(script.text, "#!/bin/sh\nexit 0\n");
}
