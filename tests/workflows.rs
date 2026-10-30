use std::fs;
use std::path::{Path, PathBuf};

use narsor_engine::domain::{ExecutionStatus, Workspace};
use narsor_engine::engine::{Engine, HumanInputDelivery};
use narsor_engine::persistence::ExecutionStore;
use narsor_engine::workflow::load_from_file;
use serde_json::{json, Map};

struct TempRoot(PathBuf);

impl TempRoot {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("narsor-template-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).expect("create temporary root");
        Self(path)
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

#[tokio::test]
async fn template_runs_a_no_op_script_then_waits_for_a_human_choice() {
    let root = TempRoot::new();
    let definition =
        load_from_file(Path::new(env!("CARGO_MANIFEST_DIR")).join("workflows/template.yaml"))
            .expect("load the built-in template");
    let workspace = root.path().join("workspace");
    fs::create_dir_all(&workspace).expect("create workspace");
    let engine = Engine::new(ExecutionStore::new(root.path().join("store")));

    let waiting = engine
        .start_and_run(
            &definition,
            json!({"example_text": "hello engine"}),
            Workspace::local_folder(workspace.to_string_lossy().into_owned()),
        )
        .await
        .expect("run the template");

    assert_eq!(waiting.status, ExecutionStatus::InputNeeded);
    assert_eq!(waiting.current_state.as_deref(), Some("review"));
    assert_eq!(
        waiting.context.variables.get("example_value"),
        Some(&json!("sample"))
    );
    assert_eq!(
        waiting
            .context
            .artifacts
            .get("example_file")
            .map(|artifact| artifact.location.as_str()),
        Some("template-example.txt")
    );
    assert!(!workspace.join("template-example.txt").exists());

    let completed = engine
        .deliver_human_input(
            &definition,
            &waiting.id,
            &HumanInputDelivery::with_values(Map::from_iter([
                ("decision".to_string(), json!("finish")),
                ("comment".to_string(), json!("looks good")),
            ]))
            .choosing("finish"),
        )
        .await
        .expect("deliver the human choice");

    assert_eq!(completed.status, ExecutionStatus::Completed);
    assert_eq!(completed.current_state.as_deref(), Some("completed"));
    assert!(!workspace.join("template-example.txt").exists());
}
