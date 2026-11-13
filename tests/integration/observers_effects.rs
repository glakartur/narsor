//! Integration tests of the observers' effect routes (U3) on the service:
//! "create a task" with an effect key, its deduplication, the task limit of
//! a run, the dry run and the provenance of what an observer creates. A
//! real `ProtocolService` over a seeded catalogue and a manual clock; no
//! daemon, no network.

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, TimeZone, Utc};
use narsor_engine::controller_protocol::{
    ClaimedObserverRun, ProtocolError, ProtocolService, RunOutcome, RunReport,
};
use narsor_engine::domain::{TaskId, WorkflowDefinition};
use narsor_engine::environment::EnvironmentCatalog;
use narsor_engine::observers::{
    effect_task_id, CreateTaskEffect, EffectRecord, EffectState, ObserverConfig, ObserverEffect,
    ObserverId, ObserverName, ObserverState, PlannedEffectKind, RunStatus,
};
use narsor_engine::orchestration::{Clock, ControlPlane, OwnerRef, TaskEventKind};
use narsor_engine::persistence::StatePaths;
use narsor_protocol::controller::{
    ObserverCreateTaskRequest, ObserverEffectOutcome, ObserverEffectResponse, OwnerCommand,
    OwnerCommandRequest,
};
use serde_json::{json, Value};

/// A clock the test moves by hand.
#[derive(Debug)]
struct ManualClock(Mutex<DateTime<Utc>>);

impl Clock for ManualClock {
    fn now(&self) -> DateTime<Utc> {
        *self.0.lock().unwrap()
    }
}

/// A seeded catalogue with a `triage` workflow, a clock, and a service.
struct Fixture {
    root: PathBuf,
    clock: Arc<ManualClock>,
    service: ProtocolService,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn owner() -> OwnerRef {
    OwnerRef::new("owner")
}

/// The `triage` workflow as the observer validates its mapping against.
fn triage_definition() -> WorkflowDefinition {
    serde_json::from_value(json!({
        "id": "triage",
        "name": "triage",
        "version": 1,
        "inputs": [
            {"name": "ticket", "required": true},
            {"name": "summary", "required": false},
        ],
        "states": {},
        "initial_state": "start",
    }))
    .unwrap()
}

/// The `triage` workflow as the catalogue serves it to the launch resolver.
fn triage_yaml() -> String {
    format!(
        "name: triage\nversion: 1\ninputs:\n  - name: ticket\n    type: string\n    \
         required: true\n  - name: summary\n    type: string\n    required: false\n\
         globals:\n  image: \"{}\"\nstates:\n  work:\n    action:\n      type: script\n      \
         command: \"true\"\n    choices: [success]\n    transitions:\n      success: done\n  \
         done:\n    description: finished\nterminal_states: [done]\n",
        crate::environment_support::workflow_image()
    )
}

impl Fixture {
    fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("narsor-obs-effects-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let resolver = crate::environment_support::seeded_resolver(&root);
        fs::write(root.join("workflows").join("triage.yaml"), triage_yaml()).unwrap();
        let clock = Arc::new(ManualClock(Mutex::new(
            Utc.with_ymd_and_hms(2026, 11, 13, 12, 0, 0).unwrap(),
        )));
        let service = ProtocolService::new(
            ControlPlane::with_clock(StatePaths::new(&root), clock.clone()),
            resolver,
        );
        Self {
            root,
            clock,
            service,
        }
    }

    fn now(&self) -> DateTime<Utc> {
        self.clock.now()
    }

    fn advance(&self, by: Duration) {
        *self.clock.0.lock().unwrap() += by;
    }

    /// An active "create a task" observer running approved version 1.
    fn observer(&self, name: &str, limit: u32, backend: Option<&str>) -> ObserverId {
        let mut config = ObserverConfig::new(
            ObserverName::parse(name).unwrap(),
            "jira",
            ObserverEffect::CreateTask(CreateTaskEffect {
                workflow_id: "triage".into(),
                workflow_version: 1,
                workspaces: Default::default(),
                backend_id: backend.map(str::to_string),
                profile: None,
                mapping: [
                    ("ticket".to_string(), "key".to_string()),
                    ("summary".to_string(), "summary".to_string()),
                ]
                .into(),
            }),
        );
        config.task_limit_per_run = limit;
        let store = self.service.observers();
        let id = store
            .create(owner(), config, Some(&triage_definition()), self.now())
            .unwrap()
            .observer_id;
        store
            .add_script(&id, "echo '{\"state\": {}}'\n", self.now())
            .unwrap();
        self.service.approve_observer_version(&id, 1).unwrap();
        id
    }

    /// Flags the due runs and claims one.
    fn claim(&self) -> ClaimedObserverRun {
        self.service.observer_pass();
        self.service
            .claim_observer_run()
            .unwrap()
            .expect("a run is due")
    }

    fn create(
        &self,
        run: &ClaimedObserverRun,
        key: &str,
        data: Value,
    ) -> Result<ObserverEffectResponse, ProtocolError> {
        self.service.observer_create_task(
            &run.run_id,
            ObserverCreateTaskRequest {
                effect_key: key.to_string(),
                data,
            },
        )
    }

    fn tasks(&self) -> usize {
        self.service.control_plane().list_tasks().unwrap().len()
    }
}

fn issue(key: &str) -> Value {
    json!({"key": key, "summary": format!("summary of {key}")})
}

fn conflict_code(error: &ProtocolError) -> &str {
    match error {
        ProtocolError::Conflict { code, .. } | ProtocolError::BadRequest { code, .. } => code,
        other => panic!("expected a coded refusal, got {other:?}"),
    }
}

#[test]
fn a_key_that_created_a_task_is_a_duplicate_in_every_later_run() {
    // Covers AE2.
    let fx = Fixture::new();
    let id = fx.observer("jira-new", 10, None);
    let run = fx.claim();

    let first = fx.create(&run, "ABC-123", issue("ABC-123")).unwrap();
    assert_eq!(first.outcome, ObserverEffectOutcome::Created);
    let task_id = effect_task_id(&id, "ABC-123").to_string();
    assert_eq!(first.task_id.as_deref(), Some(task_id.as_str()));

    let again = fx.create(&run, "ABC-123", issue("ABC-123")).unwrap();
    assert_eq!(again.outcome, ObserverEffectOutcome::Duplicate);
    assert_eq!(again.task_id.as_deref(), Some(task_id.as_str()));
    assert!(again.message.contains("duplicate"), "{}", again.message);
    assert_eq!(fx.tasks(), 1);

    // The next run, with the field changed in the source: still a duplicate.
    fx.service
        .finish_observer_run(
            &run.run_id,
            RunReport {
                outcome: RunOutcome::Exited { exit_code: 0 },
                stdout: r#"{"state": {}}"#.into(),
                stderr: String::new(),
            },
        )
        .unwrap();
    fx.advance(Duration::seconds(300));
    let next = fx.claim();
    let later = fx
        .create(
            &next,
            "ABC-123",
            json!({"key": "ABC-123", "summary": "edited"}),
        )
        .unwrap();
    assert_eq!(later.outcome, ObserverEffectOutcome::Duplicate);
    assert_eq!(fx.tasks(), 1);
    assert_eq!(
        fx.service.observers().list_runs(&id).unwrap()[1].tasks_created,
        0
    );
}

#[test]
fn a_failed_run_is_followed_by_one_that_creates_only_the_missing_tasks() {
    // Covers AE3: 3 of 5 tasks, then the script fails; the next run sends
    // all 5 keys again.
    let fx = Fixture::new();
    let id = fx.observer("jira-new", 10, None);
    let keys = ["A-1", "A-2", "A-3", "A-4", "A-5"];
    let first = fx.claim();
    for key in &keys[..3] {
        fx.create(&first, key, issue(key)).unwrap();
    }
    fx.service
        .finish_observer_run(
            &first.run_id,
            RunReport {
                outcome: RunOutcome::Exited { exit_code: 1 },
                stdout: String::new(),
                stderr: "source timed out".into(),
            },
        )
        .unwrap();
    assert_eq!(fx.service.observers().load(&id).unwrap().script_state, None);

    fx.advance(Duration::seconds(600));
    let second = fx.claim();
    let outcomes: Vec<ObserverEffectOutcome> = keys
        .iter()
        .map(|key| fx.create(&second, key, issue(key)).unwrap().outcome)
        .collect();

    assert_eq!(
        outcomes,
        [
            ObserverEffectOutcome::Duplicate,
            ObserverEffectOutcome::Duplicate,
            ObserverEffectOutcome::Duplicate,
            ObserverEffectOutcome::Created,
            ObserverEffectOutcome::Created,
        ]
    );
    assert_eq!(fx.tasks(), 5);
    let runs = fx.service.observers().list_runs(&id).unwrap();
    assert_eq!(runs[0].tasks_created, 3);
    assert_eq!(runs[1].tasks_created, 2);
}

#[test]
fn a_run_over_its_task_limit_is_refused_and_the_observer_waits_for_its_owner() {
    // Covers AE4: a wrong query yields 250 issues; the limit is 10.
    let fx = Fixture::new();
    let id = fx.observer("jira-new", 10, None);
    let run = fx.claim();

    let mut created = 0;
    let mut refused = 0;
    for index in 0..250 {
        let key = format!("BAD-{index}");
        match fx.create(&run, &key, issue(&key)) {
            Ok(response) => {
                assert_eq!(response.outcome, ObserverEffectOutcome::Created);
                created += 1;
            }
            Err(error) => {
                assert_eq!(conflict_code(&error), "observer_task_limit");
                refused += 1;
            }
        }
    }

    assert_eq!((created, refused), (10, 240));
    assert_eq!(fx.tasks(), 10);
    let record = fx.service.observers().load(&id).unwrap();
    assert_eq!(record.state, ObserverState::NeedsAttention);
    // A refused key left no effect file: a later run may create it.
    assert!(fx
        .service
        .observers()
        .load_effect(&id, "BAD-10")
        .unwrap()
        .is_none());
    // Nothing runs on schedule until the owner resumes it.
    fx.service
        .finish_observer_run(
            &run.run_id,
            RunReport {
                outcome: RunOutcome::Exited { exit_code: 0 },
                stdout: r#"{"state": {}}"#.into(),
                stderr: String::new(),
            },
        )
        .unwrap();
    fx.advance(Duration::hours(1));
    fx.service.observer_pass();
    assert!(fx.service.claim_observer_run().unwrap().is_none());
}

#[test]
fn a_dry_run_says_what_it_would_create_and_creates_nothing() {
    let fx = Fixture::new();
    let id = fx.observer("jira-new", 2, None);
    fx.service
        .request_observer_dry_run(&id, Some(1), false)
        .unwrap();
    let run = fx.service.claim_observer_run().unwrap().unwrap();
    assert!(run.dry_run);

    let would = fx.create(&run, "ABC-1", issue("ABC-1")).unwrap();
    assert_eq!(would.outcome, ObserverEffectOutcome::WouldCreate);
    let again = fx.create(&run, "ABC-1", issue("ABC-1")).unwrap();
    assert_eq!(again.outcome, ObserverEffectOutcome::Duplicate);
    fx.create(&run, "ABC-2", issue("ABC-2")).unwrap();
    // The same limit as a real run, but the observer's state stays.
    let over = fx.create(&run, "ABC-3", issue("ABC-3")).unwrap_err();
    assert_eq!(conflict_code(&over), "observer_task_limit");

    assert_eq!(fx.tasks(), 0);
    assert!(fx
        .service
        .observers()
        .load_effect(&id, "ABC-1")
        .unwrap()
        .is_none());
    let record = fx.service.observers().load(&id).unwrap();
    assert_eq!(record.state, ObserverState::Active);
    let stored = fx.service.observers().load_run(&run.run_id).unwrap();
    let planned: Vec<(&str, PlannedEffectKind)> = stored
        .planned_effects
        .iter()
        .map(|effect| (effect.effect_key.as_str(), effect.kind))
        .collect();
    assert_eq!(
        planned,
        [
            ("ABC-1", PlannedEffectKind::CreateTask),
            ("ABC-2", PlannedEffectKind::CreateTask),
        ]
    );
    assert_eq!(stored.planned_effects[0].data, issue("ABC-1"));
}

#[test]
fn an_observer_task_carries_its_provenance_and_belongs_to_the_observer_owner() {
    // R19, and the owner sees it in the task list.
    let fx = Fixture::new();
    let id = fx.observer("jira-new", 10, None);
    let run = fx.claim();
    let response = fx.create(&run, "ABC-7", issue("ABC-7")).unwrap();
    let task_id = TaskId::parse(response.task_id.unwrap()).unwrap();

    let record = fx.service.control_plane().task(&task_id).unwrap();
    assert_eq!(record.owner, owner());
    let provenance = record.provenance.clone().expect("provenance");
    assert_eq!(provenance.observer_id, id.to_string());
    assert_eq!(provenance.observer_name, "jira-new");
    assert_eq!(provenance.effect_key, "ABC-7");
    assert_eq!(provenance.run_id, run.run_id.to_string());
    assert_eq!(
        record.launch_spec.inputs,
        json!({"ticket": "ABC-7", "summary": "summary of ABC-7"})
    );
    let events = fx
        .service
        .control_plane()
        .events_since(&task_id, 0)
        .unwrap();
    assert_eq!(
        events[0].kind,
        TaskEventKind::CreatedByObserver { provenance }
    );

    let listed = fx.service.list_tasks_where(&owner(), None).unwrap();
    assert!(listed
        .tasks
        .iter()
        .any(|task| task.task_id.to_string() == task_id.to_string()));
}

#[test]
fn a_retry_after_a_crash_before_the_task_creates_it_from_the_recorded_request() {
    // The effect file was written `pending`, then the server died before
    // `create_task`; the script's retry carries changed data (the issue was
    // edited in Jira). One task, from the recorded data.
    let fx = Fixture::new();
    let id = fx.observer("jira-new", 10, None);
    let run = fx.claim();
    let ObserverEffect::CreateTask(effect) =
        fx.service.observers().load(&id).unwrap().config.effect
    else {
        panic!("a create-task observer");
    };
    let recorded = effect
        .task_request(&id, "ABC-9", &json!({"key": "ABC-9", "summary": "first"}))
        .unwrap();
    {
        let store = fx.service.observers();
        let lock = store.lock(&id).unwrap();
        store
            .create_effect(
                &lock,
                &EffectRecord::pending("ABC-9", run.run_id.sequence, recorded, fx.now()),
            )
            .unwrap();
    }

    let response = fx
        .create(&run, "ABC-9", json!({"key": "ABC-9", "summary": "edited"}))
        .unwrap();
    assert_eq!(response.outcome, ObserverEffectOutcome::Created);
    let again = fx
        .create(
            &run,
            "ABC-9",
            json!({"key": "ABC-9", "summary": "edited again"}),
        )
        .unwrap();
    assert_eq!(again.outcome, ObserverEffectOutcome::Duplicate);

    assert_eq!(fx.tasks(), 1);
    let task_id = effect_task_id(&id, "ABC-9");
    let record = fx.service.control_plane().task(&task_id).unwrap();
    assert_eq!(record.launch_spec.inputs["summary"], "first");
    let effect = fx
        .service
        .observers()
        .load_effect(&id, "ABC-9")
        .unwrap()
        .unwrap();
    assert!(
        matches!(effect.state, EffectState::Created { task_id: ref created, .. } if *created == task_id)
    );
}

#[test]
fn a_permanent_refusal_removes_the_pending_effect_so_a_later_call_tries_again() {
    // The observer pins a backend the catalogue does not have (yet).
    let fx = Fixture::new();
    let id = fx.observer("jira-new", 10, Some("later-backend"));
    let run = fx.claim();

    let refused = fx.create(&run, "ABC-1", issue("ABC-1")).unwrap_err();
    assert!(
        matches!(refused, ProtocolError::BadRequest { .. }),
        "{refused:?}"
    );
    assert!(fx
        .service
        .observers()
        .load_effect(&id, "ABC-1")
        .unwrap()
        .is_none());
    assert_eq!(
        fx.service
            .observers()
            .load_run(&run.run_id)
            .unwrap()
            .tasks_created,
        0
    );

    EnvironmentCatalog::new(&fx.root)
        .save_backend(&crate::environment_support::backend("later-backend"))
        .unwrap();
    let created = fx.create(&run, "ABC-1", issue("ABC-1")).unwrap();
    assert_eq!(created.outcome, ObserverEffectOutcome::Created);
    assert_eq!(fx.tasks(), 1);
}

#[test]
fn effect_data_outside_the_mapping_is_refused_and_records_nothing() {
    let fx = Fixture::new();
    let id = fx.observer("jira-new", 10, None);
    let run = fx.claim();

    let refused = fx
        .create(
            &run,
            "ABC-1",
            json!({"key": "ABC-1", "workflow_id": "other"}),
        )
        .unwrap_err();
    assert_eq!(conflict_code(&refused), "invalid_effect_data");
    let not_an_object = fx.create(&run, "ABC-1", json!(["ABC-1"])).unwrap_err();
    assert_eq!(conflict_code(&not_an_object), "invalid_effect_data");
    assert_eq!(fx.tasks(), 0);
    assert!(fx
        .service
        .observers()
        .load_effect(&id, "ABC-1")
        .unwrap()
        .is_none());
}

#[test]
fn a_run_that_ended_can_have_no_more_effects() {
    let fx = Fixture::new();
    let _id = fx.observer("jira-new", 10, None);
    let run = fx.claim();
    fx.service
        .finish_observer_run(
            &run.run_id,
            RunReport {
                outcome: RunOutcome::Exited { exit_code: 0 },
                stdout: r#"{"state": {}}"#.into(),
                stderr: String::new(),
            },
        )
        .unwrap();

    let refused = fx.create(&run, "ABC-1", issue("ABC-1")).unwrap_err();
    assert!(
        matches!(refused, ProtocolError::Unauthorized),
        "{refused:?}"
    );
    assert!(fx
        .service
        .authenticate_observer_run(run.token.expose())
        .is_none());
    assert_eq!(fx.tasks(), 0);
    assert_eq!(
        fx.service.observers().load_run(&run.run_id).unwrap().status,
        RunStatus::Succeeded
    );
}

#[test]
fn an_owner_key_with_the_observer_prefix_is_refused() {
    let fx = Fixture::new();
    let _id = fx.observer("jira-new", 10, None);
    let run = fx.claim();
    let task_id = fx
        .create(&run, "ABC-1", issue("ABC-1"))
        .unwrap()
        .task_id
        .unwrap();

    let refused = fx
        .service
        .command(
            &owner(),
            &task_id,
            OwnerCommandRequest {
                command: OwnerCommand::Pause,
                idempotency_key: "observer:obs-x:ABC-1".into(),
                expected_generation: None,
            },
        )
        .unwrap_err();
    match refused {
        ProtocolError::BadRequest { code, message } => {
            assert_eq!(code, "reserved_idempotency_key");
            assert!(message.contains("observer:"), "{message}");
        }
        other => panic!("expected a 400, got {other:?}"),
    }
}
