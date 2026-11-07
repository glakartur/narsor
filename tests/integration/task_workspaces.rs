//! The workspace rows a task summary carries: what the runner reported, or
//! else what the launch spec knows.

use chrono::Utc;
use narsor_engine::controller_protocol::service::task_workspaces;
use narsor_engine::domain::{
    BackendId, ExecutionStatus, GitIdentity, ResolvedEnvironment, ResourceProfile, TaskId,
    TaskLaunchSpec, TaskRepository, TaskRepositorySource, TaskWorkspace, WorkspaceHeader,
};
use narsor_engine::orchestration::{
    ControlPlane, NewTask, OwnerRef, RunnerObservation, RunnerSnapshot, TaskRecord,
};
use narsor_engine::persistence::paths::StatePaths;

struct TempRoot(std::path::PathBuf);

impl TempRoot {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("narsor-task-workspaces-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("create temp root");
        Self(path)
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn git_repository(name: &str, branch: Option<&str>) -> TaskRepository {
    let mut repository = TaskRepository::new(
        name,
        TaskRepositorySource::Git(Box::new(GitIdentity {
            repository: None,
            remote_url: format!("https://git.example/g/{name}-repo.git"),
            commit: "0123456789abcdef0123456789abcdef01234567".to_string(),
            git_ref: None,
            ref_origin: None,
            credential: None,
        })),
    );
    repository.branch = branch.map(str::to_string);
    repository
}

fn queued_task(root: &TempRoot, repositories: Vec<TaskRepository>) -> TaskRecord {
    let spec = TaskLaunchSpec::new(
        "implement-feature",
        1,
        serde_json::json!({}),
        TaskWorkspace { repositories },
        ResolvedEnvironment::harness_only("claude-code"),
        ResourceProfile::new(1000, 2048),
    );
    ControlPlane::new(StatePaths::new(&root.0))
        .create_task(NewTask::new(
            TaskId::parse("task-1").unwrap(),
            OwnerRef::new("owner"),
            BackendId::parse("k8s").unwrap(),
            spec,
        ))
        .expect("create the task")
}

fn reported(record: &mut TaskRecord, workspaces: Vec<WorkspaceHeader>) {
    record.runner = Some(RunnerObservation {
        incarnation: "pod-1".to_string(),
        launch_generation: 1,
        snapshot: RunnerSnapshot {
            snapshot_sequence: 1,
            execution_status: ExecutionStatus::Running,
            execution_cursor: Default::default(),
            event_cursor: 0,
            pending_input: None,
            conversation: Default::default(),
            timeline: None,
            safe_point: false,
            problem: None,
            attention: Vec::new(),
            conductor_available: false,
            conductor_unavailable_reason: None,
            default_harness_unavailable: None,
            workspaces,
            generated_title: None,
            observed_at: Utc::now(),
        },
        received_at: Utc::now(),
        safe_point_generation: None,
    });
}

#[test]
fn five_repositories_give_five_rows_in_the_order_of_the_launch_spec() {
    let root = TempRoot::new();
    let names = ["code", "docs", "api", "web", "ops"];
    let record = queued_task(
        &root,
        names
            .iter()
            .map(|name| git_repository(name, Some("main")))
            .collect(),
    );

    let rows = task_workspaces(&record);

    let found: Vec<_> = rows.iter().map(|row| row.name.as_str()).collect();
    assert_eq!(found, names);
}

#[test]
fn a_queued_task_shows_the_repository_the_start_branch_and_no_changes_or_pull_request() {
    let root = TempRoot::new();
    let record = queued_task(&root, vec![git_repository("code", Some("release/2.0"))]);

    let row = task_workspaces(&record).remove(0);

    assert_eq!(row.repo, "code-repo");
    assert_eq!(row.branch.as_deref(), Some("release/2.0"));
    assert_eq!(row.base_branch.as_deref(), Some("release/2.0"));
    assert_eq!((row.added, row.removed), (None, None));
    assert_eq!((row.pr, row.pr_url), (None, None));
}

#[test]
fn a_repository_without_a_known_branch_shows_only_its_name() {
    let root = TempRoot::new();
    let record = queued_task(&root, vec![git_repository("code", None)]);

    let row = task_workspaces(&record).remove(0);

    assert_eq!(row.repo, "code-repo");
    assert_eq!((row.branch, row.base_branch), (None, None));
}

#[test]
fn the_report_of_a_runner_replaces_the_launch_rows_and_is_cleaned() {
    let root = TempRoot::new();
    let mut record = queued_task(&root, vec![git_repository("code", Some("main"))]);
    reported(
        &mut record,
        vec![WorkspaceHeader {
            name: "code".into(),
            repo: "orders\u{1b}[31m-api".into(),
            branch: Some("feat/x".into()),
            base_branch: Some("main".into()),
            added: Some(3),
            removed: Some(1),
            pr: Some("42".into()),
            pr_url: Some("javascript:alert(1)".into()),
        }],
    );

    let row = task_workspaces(&record).remove(0);

    assert_eq!(row.repo, "orders[31m-api");
    assert_eq!(row.branch.as_deref(), Some("feat/x"));
    assert_eq!((row.added, row.removed), (Some(3), Some(1)));
    assert_eq!(row.pr.as_deref(), Some("42"));
    assert_eq!(row.pr_url, None);
}

#[test]
fn a_report_without_workspaces_leaves_the_launch_rows_in_place() {
    let root = TempRoot::new();
    let mut record = queued_task(&root, vec![git_repository("code", Some("main"))]);
    reported(&mut record, Vec::new());

    let rows = task_workspaces(&record);

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].repo, "code-repo");
}
