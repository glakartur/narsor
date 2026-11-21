//! Integration tests of the automations' runtime store: the record of a
//! (queue, automation) pair, run errors, queue entries and the script
//! contract.
//!
//! Every test runs against a real temporary directory: the value of the
//! store lies in real file operations, locks and exclusive creates.

use chrono::{DateTime, Duration, TimeZone, Utc};
use narsor_engine::automations::HookPhase;
use narsor_engine::domain::TaskId;
use narsor_engine::observers::{
    parse_script_output, queue_entry_id, stored_observers_warning, ActiveRun, AutomationKey,
    AutomationRecord, AutomationState, ContractError, EntryWrite, ObserverError, ObserverStore,
    QueueEntry, RunEntry, RunFailure, RunStatus, ScriptInput, AUTOMATION_RECORD_VERSION,
    KEPT_RUN_FAILURES, MAX_SCRIPT_OUTPUT_BYTES, RUN_FAILURE_VERSION, SCRIPT_CONTRACT_VERSION,
};
use narsor_engine::orchestration::OwnerRef;
use narsor_engine::persistence::paths::temp_path_for;
use narsor_engine::persistence::ServerStatePaths;
use narsor_protocol::controller::{GateWatch, GateWatchChoice, GateWatchVariable};
use serde_json::json;
use std::fs;
use std::path::PathBuf;

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
        ObserverStore::new(ServerStatePaths::new(&self.path))
    }

    fn paths(&self) -> ServerStatePaths {
        ServerStatePaths::new(&self.path)
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 18, 12, 0, 0).unwrap()
}

fn owner() -> OwnerRef {
    OwnerRef::new("owner")
}

fn key(queue: &str, automation: &str) -> AutomationKey {
    AutomationKey::new(queue, automation).unwrap()
}

fn active_run(sequence: u64) -> ActiveRun {
    ActiveRun {
        sequence,
        phase: HookPhase::Check,
        script_digest: "sha256:0123".into(),
        definition_digest: "sha256:4567".into(),
        claimed_at: now(),
        deadline: now() + Duration::seconds(360),
        token_digest: "sha256:abc".into(),
        tasks_created: 2,
        entry: None,
        exit_generation: None,
    }
}

fn failure(key: &AutomationKey, sequence: u64) -> RunFailure {
    RunFailure {
        record_version: RUN_FAILURE_VERSION,
        key: key.clone(),
        sequence,
        phase: HookPhase::Check,
        script_digest: "sha256:0123".into(),
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

/// Saves a new record of `key` with a run in progress and returns it.
fn saved(store: &ObserverStore, key: &AutomationKey) -> AutomationRecord {
    let lock = store.lock(key).unwrap();
    let mut record = store.load_or_new(key, now()).unwrap();
    record.active_run = Some(active_run(1));
    store.save(&lock, &mut record, now()).unwrap();
    record
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

// ---------------------------------------------------------------------------
// The record of a pair
// ---------------------------------------------------------------------------

#[test]
fn a_pair_has_no_record_until_its_first_save_and_each_save_is_one_revision() {
    let root = TempRoot::new();
    let store = root.store();
    let pair = key("mr.review", "gitlab-mr.label");
    assert_eq!(store.load(&pair).unwrap(), None);

    let first = saved(&store, &pair);
    let mut loaded = store.load(&pair).unwrap().unwrap();
    assert_eq!(loaded, first);
    assert_eq!(loaded.revision, 1);
    assert_eq!(loaded.state, AutomationState::Active);
    assert_eq!(loaded.active_run, Some(active_run(1)));

    let lock = store.lock(&pair).unwrap();
    store.save(&lock, &mut loaded, now()).unwrap();
    assert_eq!(store.load(&pair).unwrap().unwrap().revision, 2);
    // The file sits under the queue entries' directory, not the
    // catalogue's `automations/` of a shared root.
    let file = root
        .paths()
        .automation_state_file("mr.review", "gitlab-mr.label");
    assert!(
        file.starts_with(root.paths().observer_queues_dir()),
        "{}",
        file.display()
    );
    let raw: serde_json::Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    assert_eq!(raw["queue"], "mr.review");
    assert_eq!(raw["automation"], "gitlab-mr.label");
    assert_eq!(raw["record_version"], AUTOMATION_RECORD_VERSION);
}

#[test]
fn a_record_or_an_error_needs_its_own_pairs_lock() {
    let root = TempRoot::new();
    let store = root.store();
    let pair = key("mr-review", "a");
    let other = store.lock(&key("mr-review", "b")).unwrap();
    let mut record = AutomationRecord::new(pair.clone(), now());

    assert!(matches!(
        store.save(&other, &mut record, now()),
        Err(ObserverError::LockMismatch { .. })
    ));
    assert!(matches!(
        store.record_failure(&other, &failure(&pair, 1)),
        Err(ObserverError::LockMismatch { .. })
    ));
    assert_eq!(store.load(&pair).unwrap(), None);
    assert!(store.list_failures(&pair).unwrap().is_empty());
}

#[test]
fn deleting_a_record_removes_its_errors_and_the_empty_queue_directory() {
    let root = TempRoot::new();
    let store = root.store();
    let pair = key("mr-review", "gitlab-mr-label");
    saved(&store, &pair);
    let lock = store.lock(&pair).unwrap();
    store.record_failure(&lock, &failure(&pair, 1)).unwrap();

    assert!(store.delete(&lock).unwrap());
    drop(lock);

    assert_eq!(store.load(&pair).unwrap(), None);
    assert!(store.list_failures(&pair).unwrap().is_empty());
    assert!(!root.paths().automation_queue_dir("mr-review").exists());
    let lock = store.lock(&pair).unwrap();
    assert!(!store.delete(&lock).unwrap(), "nothing left to delete");
}

#[test]
fn the_listing_skips_unreadable_records_and_directories_without_one() {
    // D7: one bad file stops no other pair.
    let root = TempRoot::new();
    let store = root.store();
    let healthy = saved(&store, &key("q", "healthy"));
    saved(&store, &key("q", "corrupt"));
    saved(&store, &key("q", "newer"));
    fs::write(
        root.paths().automation_state_file("q", "corrupt"),
        b"{ not json",
    )
    .unwrap();
    let newer = root.paths().automation_state_file("q", "newer");
    let mut raw: serde_json::Value = serde_json::from_slice(&fs::read(&newer).unwrap()).unwrap();
    raw["record_version"] = json!(AUTOMATION_RECORD_VERSION + 1);
    fs::write(&newer, serde_json::to_vec(&raw).unwrap()).unwrap();
    fs::create_dir_all(root.paths().automation_state_dir("q", "no-record")).unwrap();

    assert_eq!(store.list_readable().unwrap(), vec![healthy]);
    assert!(matches!(
        store.load(&key("q", "newer")),
        Err(ObserverError::UnsupportedRecordVersion { .. })
    ));
}

#[test]
fn a_state_from_a_newer_build_reads_as_unrecognized() {
    let state: AutomationState = serde_json::from_value(json!("hibernating")).unwrap();
    assert_eq!(state, AutomationState::Unrecognized);
    assert!(!state.is_stopped());
}

#[test]
fn errors_are_kept_newest_first_up_to_the_limit_and_one_per_entry() {
    // R23: a newer error of an entry replaces the older one.
    let root = TempRoot::new();
    let store = root.store();
    let pair = key("jira-intake", "jira-new");
    let lock = store.lock(&pair).unwrap();
    let written = KEPT_RUN_FAILURES as u64 + 5;
    for sequence in 1..=written {
        store
            .record_failure(&lock, &failure(&pair, sequence))
            .unwrap();
    }
    let kept = store.list_failures(&pair).unwrap();
    assert_eq!(kept.len(), KEPT_RUN_FAILURES);
    assert_eq!(kept[0], failure(&pair, written));
    assert_eq!(kept.last().unwrap().sequence, 6);

    let with_entry = |sequence, entry_id: &str| RunFailure {
        entry_id: Some(entry_id.to_string()),
        ..failure(&pair, sequence)
    };
    store.record_failure(&lock, &with_entry(100, "a")).unwrap();
    store.record_failure(&lock, &with_entry(101, "a")).unwrap();
    let of_a: Vec<u64> = store
        .list_failures(&pair)
        .unwrap()
        .into_iter()
        .filter(|failure| failure.entry_id.as_deref() == Some("a"))
        .map(|failure| failure.sequence)
        .collect();
    assert_eq!(of_a, vec![101]);
}

#[test]
fn the_run_of_an_entry_names_its_task_and_episode() {
    let root = TempRoot::new();
    let store = root.store();
    let pair = key("mr-review", "gitlab-mr-label");
    let lock = store.lock(&pair).unwrap();
    let checked = entry(owner(), "mr-review", "task-1", "input-1");
    let mut run = active_run(1);
    run.entry = Some(RunEntry {
        entry_id: checked.entry_id.clone(),
        task_id: checked.task_id.clone(),
        episode: checked.episode.clone(),
    });
    let mut record = AutomationRecord::new(pair.clone(), now());
    record.active_run = Some(run.clone());
    record.entry_round = vec![checked.entry_id.clone()];

    store.save(&lock, &mut record, now()).unwrap();

    let loaded = store.load(&pair).unwrap().unwrap();
    assert_eq!(loaded.active_run, Some(run));
    assert_eq!(loaded.entry_round, vec![checked.entry_id]);
}

#[test]
fn records_errors_and_entries_survive_a_restart_and_stray_writes_are_swept() {
    let root = TempRoot::new();
    let pair = key("mr-review", "gitlab-mr-label");
    let (record, error, entry) = {
        let store = root.store();
        let record = saved(&store, &pair);
        let lock = store.lock(&pair).unwrap();
        let error = failure(&pair, 1);
        store.record_failure(&lock, &error).unwrap();
        drop(lock);
        let lock = store.lock_queue_entries().unwrap();
        let entry = self::entry(owner(), "mr-review", "task-1", "input-1");
        store.add_queue_entry(&lock, &entry).unwrap();
        (record, error, entry)
    };
    let stray = temp_path_for(
        &root
            .paths()
            .automation_state_file("mr-review", "gitlab-mr-label"),
    );
    fs::write(&stray, b"{").unwrap();
    let stray_entry = temp_path_for(&root.paths().queue_entry_file(&entry.entry_id));
    fs::write(&stray_entry, b"{").unwrap();

    let store = root.store();

    let mut swept = store.cleanup_orphaned_temp_files().unwrap();
    swept.sort();
    let mut expected = vec![stray, stray_entry];
    expected.sort();
    assert_eq!(swept, expected);
    assert_eq!(store.list_readable().unwrap(), vec![record]);
    assert_eq!(store.list_failures(&pair).unwrap(), vec![error]);
    assert_eq!(
        store.queue_entries(&owner(), "mr-review").unwrap(),
        vec![entry]
    );
}

#[test]
fn stored_observers_of_an_older_version_are_only_reported() {
    // R22: no migration; the record is never read, the server warns once.
    let root = TempRoot::new();
    let old = root.paths().observers_dir().join("obs-1");
    fs::create_dir_all(&old).unwrap();
    fs::write(old.join("observer.json"), br#"{"record_version": 2}"#).unwrap();

    let warning = stored_observers_warning(&root.paths()).expect("a warning");
    assert!(warning.contains("ignored"), "{warning}");
    assert!(root.store().list_readable().unwrap().is_empty());
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
fn the_script_input_of_version_three_names_its_phase_queue_and_automation() {
    let input = ScriptInput {
        contract_version: SCRIPT_CONTRACT_VERSION,
        phase: HookPhase::Check,
        queue: "jira-intake".into(),
        automation: "jira-new".into(),
        condition: json!({"jql": "project = ABC"}),
        state: Some(json!({"cursor": 4})),
        entry: None,
        exit: None,
        first_run: true,
        import_backlog: false,
    };

    let value: serde_json::Value = serde_json::from_slice(&input.to_json_bytes()).unwrap();

    assert_eq!(value["contract_version"], json!(3));
    assert_eq!(value["phase"], "check");
    assert_eq!(value["queue"], "jira-intake");
    assert_eq!(value["automation"], "jira-new");
    assert!(value.get("observer").is_none());
    // R25: an `always` run checks no entry; only an `out` run has an exit.
    assert!(value.get("entry").is_none(), "{value}");
    assert!(value.get("exit").is_none(), "{value}");
    let read: ScriptInput = serde_json::from_value(value).unwrap();
    assert_eq!(read, input);
}
