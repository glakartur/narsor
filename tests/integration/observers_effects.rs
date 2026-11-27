//! Integration tests of the effect routes of an automation's run (U3) on
//! the service: "create a task" (the owner's request, no deduplication on
//! the server) and the provenance of what a script creates. A real
//! `ProtocolService` over a seeded catalogue (the `always` queue
//! `jira-intake` served by `jira-new`, and a `triage` workflow) and a
//! manual clock; no daemon, no network.

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, TimeZone, Utc};
use narsor_engine::controller_protocol::{
    ClaimedObserverRun, ProtocolError, ProtocolService, RunOutcome, RunReport,
};
use narsor_engine::domain::TaskId;
use narsor_engine::environment::EnvironmentCatalog;
use narsor_engine::observers::{AutomationKey, AutomationRecord, RunStatus};
use narsor_engine::orchestration::{Clock, ControlPlane, OwnerRef, TaskEventKind};
use narsor_engine::persistence::ServerStatePaths;
use narsor_protocol::controller::{
    CreateTaskRequest, ObserverEffectOutcome, ObserverEffectResponse, OwnerCommand,
    OwnerCommandRequest,
};
use serde_json::{json, Value};

use crate::automation_support::{always_queue, automation, write, CHECK_SCRIPT};

const QUEUE: &str = "jira-intake";
const AUTOMATION: &str = "jira-new";

/// A clock the test moves by hand.
#[derive(Debug)]
struct ManualClock(Mutex<DateTime<Utc>>);

impl Clock for ManualClock {
    fn now(&self) -> DateTime<Utc> {
        *self.0.lock().unwrap()
    }
}

/// A seeded catalogue with a `triage` workflow and the queue
/// [`QUEUE`], a manual clock, and a service.
struct Fixture {
    root: PathBuf,
    service: ProtocolService,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// The owner of the installation the tests' service names.
fn installation_owner() -> OwnerRef {
    OwnerRef::new("installation-owner")
}

/// The `triage` workflow as the catalogue serves it.
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
    /// [`QUEUE`] in an installation owned by [`installation_owner`].
    fn new() -> Self {
        Self::owned_by(Some(installation_owner()))
    }

    fn owned_by(owner: Option<OwnerRef>) -> Self {
        let root =
            std::env::temp_dir().join(format!("narsor-obs-effects-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let resolver = crate::environment_support::seeded_resolver(&root);
        write(&root, "workflows/triage.yaml", &triage_yaml());
        automation(&root, AUTOMATION, CHECK_SCRIPT, &[]);
        always_queue(&root, QUEUE, AUTOMATION, "{interval_secs: 300}");
        let clock = Arc::new(ManualClock(Mutex::new(
            Utc.with_ymd_and_hms(2026, 11, 13, 12, 0, 0).unwrap(),
        )));
        let service = ProtocolService::new(
            ControlPlane::with_clock(ServerStatePaths::new(&root), clock),
            resolver,
        )
        .with_installation_owner(Arc::new(move || owner.clone()));
        Self { root, service }
    }

    fn key() -> AutomationKey {
        AutomationKey::new(QUEUE, AUTOMATION).unwrap()
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
        request: Value,
    ) -> Result<ObserverEffectResponse, ProtocolError> {
        let request: CreateTaskRequest = serde_json::from_value(request).unwrap();
        self.service.observer_create_task(&run.run_id, request)
    }

    fn finish(&self, run: &ClaimedObserverRun) {
        self.service
            .finish_observer_run(
                &run.run_id,
                RunReport {
                    outcome: RunOutcome::Exited { exit_code: 0 },
                    stdout: r#"{"state": {}}"#.into(),
                    stderr: String::new(),
                },
            )
            .unwrap();
    }

    fn record(&self) -> AutomationRecord {
        self.service
            .observers()
            .load(&Self::key())
            .unwrap()
            .expect("the pair's record")
    }

    fn tasks(&self) -> usize {
        self.service.control_plane().list_tasks().unwrap().len()
    }
}

/// The request `narsor-cli task create triage --ticket KEY --input …`
/// sends: no version, so the catalogue's.
fn issue(key: &str) -> Value {
    json!({"launch": {
        "workflow_id": "triage",
        "workflow_version": 0,
        "inputs": {"ticket": key, "summary": format!("summary of {key}")},
        "ticket": key,
    }})
}

fn conflict_code(error: &ProtocolError) -> &str {
    match error {
        ProtocolError::Conflict { code, .. } | ProtocolError::BadRequest { code, .. } => code,
        other => panic!("expected a coded refusal, got {other:?}"),
    }
}

#[test]
fn every_call_creates_a_new_task_with_a_random_id() {
    // D17: the server does not deduplicate; the script keeps track in its
    // state of what it already reported.
    let fx = Fixture::new();
    let run = fx.claim();

    let first = fx.create(&run, issue("ABC-123")).unwrap();
    let again = fx.create(&run, issue("ABC-123")).unwrap();

    assert_eq!(first.outcome, ObserverEffectOutcome::Created);
    assert_eq!(again.outcome, ObserverEffectOutcome::Created);
    let first_id = first.task_id.expect("a task id");
    let again_id = again.task_id.expect("a task id");
    assert_ne!(first_id, again_id);
    assert!(!first_id.starts_with(QUEUE), "{first_id}");
    assert!(first.message.contains(&first_id), "{}", first.message);
    assert_eq!(fx.tasks(), 2);
}

#[test]
fn a_queue_task_carries_its_provenance_and_belongs_to_the_installation_owner() {
    // R19, A4: the owner sees it in the task list.
    let fx = Fixture::new();
    let run = fx.claim();
    let response = fx.create(&run, issue("ABC-7")).unwrap();
    let task_id = TaskId::parse(response.task_id.unwrap()).unwrap();

    let record = fx.service.control_plane().task(&task_id).unwrap();
    assert_eq!(record.owner, installation_owner());
    let provenance = record.provenance.clone().expect("provenance");
    assert_eq!(provenance.observer_id, QUEUE);
    assert_eq!(provenance.observer_name, AUTOMATION);
    assert_eq!(provenance.run_id, run.run_id.to_string());
    assert_eq!(
        record.launch_spec.inputs,
        json!({"ticket": "ABC-7", "summary": "summary of ABC-7"})
    );
    assert_eq!(record.launch_spec.ticket.as_deref(), Some("ABC-7"));
    assert_eq!(
        record.launch_spec.workflow_version, 1,
        "the catalogue's version"
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

    let listed = fx
        .service
        .list_tasks_where(&installation_owner(), None)
        .unwrap();
    assert!(listed
        .tasks
        .iter()
        .any(|task| task.task_id.to_string() == task_id.to_string()));
}

#[test]
fn an_installation_with_no_owner_yet_creates_no_task() {
    let fx = Fixture::owned_by(None);
    let run = fx.claim();

    let refused = fx.create(&run, issue("ABC-1")).unwrap_err();

    assert_eq!(conflict_code(&refused), "no_installation_owner");
    assert_eq!(fx.tasks(), 0);
}

#[test]
fn a_refused_request_creates_nothing_and_a_later_call_tries_again() {
    // The request pins a backend the catalogue does not have (yet), then
    // names a workflow the catalogue does not have.
    let fx = Fixture::new();
    let run = fx.claim();
    let mut on_later_backend = issue("ABC-1");
    on_later_backend["backend_id"] = json!("later-backend");
    let mut unknown_workflow = issue("ABC-1");
    unknown_workflow["launch"]["workflow_id"] = json!("no-such-workflow");

    let refused = fx.create(&run, on_later_backend.clone()).unwrap_err();
    let unknown = fx.create(&run, unknown_workflow).unwrap_err();

    assert!(
        matches!(refused, ProtocolError::BadRequest { .. }),
        "{refused:?}"
    );
    assert!(
        matches!(unknown, ProtocolError::BadRequest { .. }),
        "{unknown:?}"
    );
    assert_eq!(fx.tasks(), 0);
    EnvironmentCatalog::new(&fx.root)
        .save_backend(&crate::environment_support::backend("later-backend"))
        .unwrap();
    let created = fx.create(&run, on_later_backend).unwrap();
    assert_eq!(created.outcome, ObserverEffectOutcome::Created);
    assert_eq!(fx.tasks(), 1);
}

#[test]
fn a_run_that_ended_can_have_no_more_effects() {
    let fx = Fixture::new();
    let run = fx.claim();
    fx.finish(&run);

    let refused = fx.create(&run, issue("ABC-1")).unwrap_err();
    assert!(
        matches!(refused, ProtocolError::Unauthorized),
        "{refused:?}"
    );
    assert!(fx
        .service
        .authenticate_observer_run(run.token.expose())
        .is_none());
    assert_eq!(fx.tasks(), 0);
    let record = fx.record();
    assert_eq!(record.active_run, None);
    assert_eq!(record.last_run.unwrap().status, RunStatus::Succeeded);
}

#[test]
fn an_owner_key_with_the_observer_prefix_is_refused() {
    let fx = Fixture::new();
    let run = fx.claim();
    let task_id = fx.create(&run, issue("ABC-1")).unwrap().task_id.unwrap();

    let refused = fx
        .service
        .command(
            &installation_owner(),
            &task_id,
            OwnerCommandRequest {
                command: OwnerCommand::Pause,
                idempotency_key: "observer:jira-intake~jira-new~1:ABC-1".into(),
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
