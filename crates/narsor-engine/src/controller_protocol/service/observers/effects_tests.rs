//! The "deliver an event" effect on the service (R4, R19, AE7): what
//! goes to a task's history, what is queued for its waiting `wait` step,
//! and what is refused. The waiting state is written into the task record
//! by hand, as the runner's report will (U4).

use std::sync::Arc;

use serde_json::json;

use super::tests::{Fixture, PREFIX};
use crate::controller_protocol::error::ProtocolError;
use crate::controller_protocol::service::ProtocolService;
use crate::controller_protocol::wire::*;
use crate::domain::{
    BackendId, GitIdentity, ResolvedEnvironment, ResourceProfile, TaskId, TaskLaunchSpec,
    TaskRepository, TaskRepositorySource, TaskWorkspace,
};
use crate::environment::{
    BackendRecord, LaunchError, LaunchIntent, LaunchResolver, ResolvedLaunch, SlotReleaseTimings,
};
use crate::observers::{ObserverId, WatchMode};
use crate::orchestration::{
    ControlPlane, EventWait, InputStatus, NewTask, OwnerRef, RunnerState, TaskEvent,
};
use crate::persistence::StatePaths;

/// A resolver for which every backend is available; it launches nothing.
struct AnyBackend;

impl LaunchResolver for AnyBackend {
    fn resolve(&self, _intent: &LaunchIntent) -> Result<ResolvedLaunch, LaunchError> {
        Err(LaunchError::UnknownBackend("none".to_string()))
    }

    fn check_backend(&self, _backend: &BackendId) -> Result<(), LaunchError> {
        Ok(())
    }

    fn slot_release_timings(&self, backend: &BackendId) -> Result<SlotReleaseTimings, LaunchError> {
        Err(LaunchError::UnknownBackend(backend.to_string()))
    }

    fn configured_default_backend(&self) -> String {
        "k8s".to_string()
    }

    fn backend_record(&self, _backend: &BackendId) -> Result<Option<BackendRecord>, LaunchError> {
        Ok(None)
    }
}

/// A watch-list observer with a claimed run, and a task on its list.
struct Delivery {
    fx: Fixture,
    observer: ObserverId,
    run: crate::observers::RunId,
    task: TaskId,
}

impl Delivery {
    fn new() -> Self {
        let fx = Fixture::new();
        let observer = fx.active("mr-review", WatchMode::WatchList);
        let task = fx.task("task-1");
        fx.watch(&observer, &task, &format!("{PREFIX}a/-/merge_requests/17"));
        fx.service.observer_pass();
        let run = fx.claim().expect("a run").run_id;
        Self {
            fx,
            observer,
            run,
            task,
        }
    }

    fn deliver(
        &self,
        task: &TaskId,
        event_type: &str,
        data: serde_json::Value,
    ) -> Result<ObserverEffectResponse, ProtocolError> {
        self.fx.service.observer_deliver_event(
            &self.run,
            task.as_str(),
            ObserverEventRequest {
                event_type: event_type.to_string(),
                data,
            },
        )
    }

    /// Makes the task wait in a `wait` step, as its runner reports it.
    fn wait_for(&self, event_type: &str, filter: serde_json::Value) {
        let control = self.fx.service.control_plane();
        let _lock = control.store().lock_task(&self.task).unwrap();
        let mut record = control.task(&self.task).unwrap();
        record.event_wait = Some(EventWait {
            episode_id: "episode-1".into(),
            event_type: event_type.into(),
            filter,
            since: self.fx.now(),
            watch: None,
        });
        control.store().save_task(&mut record).unwrap();
    }

    fn external_events(&self) -> Vec<TaskEvent> {
        self.fx
            .service
            .control_plane()
            .events_since(&self.task, 0)
            .unwrap()
            .into_iter()
            .filter(|event| matches!(event.kind, TaskEventKind::ExternalEventReceived { .. }))
            .collect()
    }
}

fn coded(error: &ProtocolError) -> &str {
    match error {
        ProtocolError::BadRequest { code, .. } | ProtocolError::Conflict { code, .. } => code,
        other => panic!("expected a coded refusal, got {other:?}"),
    }
}

/// A task of `owner` the observer's watch list can point at.
fn task_of(fx: &Fixture, id: &str, owner: OwnerRef) -> TaskId {
    let workspace = TaskWorkspace::single(TaskRepository::new(
        "narsor",
        TaskRepositorySource::Git(Box::new(GitIdentity {
            repository: Some("narsor".to_string()),
            remote_url: "https://example.com/narsor.git".to_string(),
            commit: "614c3a8".repeat(5) + "abcde",
            git_ref: None,
            ref_origin: None,
            credential: None,
        })),
    ));
    let spec = TaskLaunchSpec::new(
        "steps",
        1,
        json!({}),
        workspace,
        ResolvedEnvironment::harness_only("claude-code"),
        ResourceProfile::new(1000, 2048),
    );
    let task_id = TaskId::parse(id).unwrap();
    fx.service
        .control_plane()
        .create_task(NewTask::new(
            task_id.clone(),
            owner,
            BackendId::parse("k8s").unwrap(),
            spec,
        ))
        .unwrap();
    task_id
}

#[test]
fn an_event_for_a_task_that_does_not_wait_only_goes_to_its_history() {
    // Covers AE7 (the history part).
    let d = Delivery::new();
    let before = d.fx.service.control_plane().task(&d.task).unwrap();

    let response = d.deliver(&d.task, "approved", json!({"mr": 17})).unwrap();

    assert_eq!(response.outcome, ObserverEffectOutcome::Recorded);
    assert_eq!(response.input_id, None);
    let after = d.fx.service.control_plane().task(&d.task).unwrap();
    assert_eq!(after.runner_state, before.runner_state);
    assert_eq!(after.generation, before.generation);
    assert!(after.event_inputs.is_empty());
    assert!(!after.owner_work_pending());
    let events = d.external_events();
    assert_eq!(events.len(), 1);
    let TaskEventKind::ExternalEventReceived {
        event_type,
        source,
        provenance,
        data,
        input_id,
    } = &events[0].kind
    else {
        unreachable!()
    };
    assert_eq!(event_type, "approved");
    assert_eq!(source, "jira");
    assert_eq!(data, &json!({"mr": 17}));
    assert_eq!(input_id, &None);
    // R19: the observer and the run the event came from.
    assert_eq!(provenance.observer_id, d.observer.to_string());
    assert_eq!(provenance.observer_name, "mr-review");
    assert_eq!(provenance.run_id, d.run.to_string());
}

#[test]
fn a_matching_event_for_a_waiting_task_is_queued_for_its_step() {
    // The filter rendered from a template is text; the event's field a
    // number: they match as text.
    let d = Delivery::new();
    d.wait_for("approved", json!({"mr": "17"}));

    let response = d
        .deliver(&d.task, "approved", json!({"mr": 17, "by": "alice"}))
        .unwrap();

    assert_eq!(response.outcome, ObserverEffectOutcome::Delivered);
    assert_eq!(response.input_id.as_deref(), Some("event-1"));
    let record = d.fx.service.control_plane().task(&d.task).unwrap();
    assert_eq!(record.event_inputs.len(), 1);
    let input = &record.event_inputs[0];
    assert_eq!(input.episode_id, "episode-1");
    assert_eq!(input.status, InputStatus::Queued);
    assert_eq!(input.data, Some(json!({"mr": 17, "by": "alice"})));
    // The server makes the key: reserved prefix, the run, a random part.
    assert!(
        input
            .idempotency_key
            .starts_with(&format!("observer:{}:", d.run)),
        "{}",
        input.idempotency_key
    );
    assert_eq!(input.provenance.run_id, d.run.to_string());
    // The slot is not freed under it, and a stranded resume finds its key.
    assert!(record.owner_work_pending());
    assert_eq!(
        record.pending_work_key(),
        Some(input.idempotency_key.as_str())
    );
    let events = d.external_events();
    assert_eq!(events.len(), 1);
    assert!(matches!(
        &events[0].kind,
        TaskEventKind::ExternalEventReceived { input_id: Some(id), .. } if id == "event-1"
    ));
}

#[test]
fn an_event_of_another_type_or_outside_the_filter_only_goes_to_the_history() {
    let d = Delivery::new();
    d.wait_for("approved", json!({"mr": "17"}));

    let other_type = d.deliver(&d.task, "commented", json!({"mr": 17})).unwrap();
    let other_mr = d.deliver(&d.task, "approved", json!({"mr": 18})).unwrap();
    let no_field = d.deliver(&d.task, "approved", json!({})).unwrap();

    for response in [&other_type, &other_mr, &no_field] {
        assert_eq!(response.outcome, ObserverEffectOutcome::Recorded);
    }
    let record = d.fx.service.control_plane().task(&d.task).unwrap();
    assert!(record.event_inputs.is_empty());
    assert_eq!(d.external_events().len(), 3);
}

#[test]
fn the_server_does_not_deduplicate_events() {
    // The script keeps track of what it sent; two calls are two events.
    let d = Delivery::new();
    let first = d.deliver(&d.task, "approved", json!({"mr": 17})).unwrap();
    let again = d.deliver(&d.task, "approved", json!({"mr": 17})).unwrap();

    assert_eq!(first.outcome, ObserverEffectOutcome::Recorded);
    assert_eq!(again.outcome, ObserverEffectOutcome::Recorded);
    assert_eq!(d.external_events().len(), 2);
}

#[test]
fn a_task_outside_the_watch_list_or_of_another_owner_is_refused() {
    let d = Delivery::new();
    let unwatched = d.fx.task("task-2");
    let refused = d.deliver(&unwatched, "approved", json!({})).unwrap_err();
    assert_eq!(coded(&refused), "task_not_watched");

    let foreign = task_of(&d.fx, "task-foreign", OwnerRef::new("someone-else"));
    d.fx.watch(
        &d.observer,
        &foreign,
        &format!("{PREFIX}a/-/merge_requests/19"),
    );
    let hidden = d.deliver(&foreign, "approved", json!({})).unwrap_err();
    assert!(matches!(hidden, ProtocolError::NotFound), "{hidden:?}");

    let bad_id =
        d.fx.service
            .observer_deliver_event(
                &d.run,
                "../task-1",
                ObserverEventRequest {
                    event_type: "approved".into(),
                    data: json!({}),
                },
            )
            .unwrap_err();
    assert!(matches!(bad_id, ProtocolError::BadRequest { .. }));
    for task in [&unwatched, &foreign] {
        let events = d.fx.service.control_plane().events_since(task, 0).unwrap();
        assert!(!events
            .iter()
            .any(|event| matches!(event.kind, TaskEventKind::ExternalEventReceived { .. })));
    }
}

#[test]
fn an_invalid_event_is_refused_before_anything_is_recorded() {
    let d = Delivery::new();
    let spaced = d
        .deliver(&d.task, "review approved", json!({}))
        .unwrap_err();
    assert_eq!(coded(&spaced), "invalid_effect_data");
    let empty_type = d.deliver(&d.task, "", json!({})).unwrap_err();
    assert_eq!(coded(&empty_type), "invalid_effect_data");
    let huge = d
        .deliver(
            &d.task,
            "approved",
            json!({"text": "x".repeat(super::MAX_EVENT_DATA_BYTES)}),
        )
        .unwrap_err();
    assert_eq!(coded(&huge), "invalid_effect_data");
    assert!(d.external_events().is_empty());
}

#[test]
fn an_observer_only_has_the_effect_its_record_names() {
    let fx = Fixture::new();
    let events_only = fx.active("mr-review", WatchMode::WatchList);
    let tasks_only = fx.active("jira-new", WatchMode::All);
    let task = fx.task("task-1");
    fx.watch(
        &events_only,
        &task,
        &format!("{PREFIX}a/-/merge_requests/17"),
    );
    fx.service.observer_pass();
    let first = fx.claim().unwrap();
    let second = fx.claim().unwrap();
    let (events_run, tasks_run) = if first.run_id.observer_id == events_only {
        (first.run_id, second.run_id)
    } else {
        (second.run_id, first.run_id)
    };
    assert_eq!(tasks_run.observer_id, tasks_only);

    let create = fx
        .service
        .observer_create_task(&events_run, ObserverCreateTaskRequest { data: json!({}) })
        .unwrap_err();
    assert_eq!(coded(&create), "effect_not_allowed");
    let event = fx
        .service
        .observer_deliver_event(
            &tasks_run,
            task.as_str(),
            ObserverEventRequest {
                event_type: "approved".into(),
                data: json!({}),
            },
        )
        .unwrap_err();
    assert_eq!(coded(&event), "effect_not_allowed");
}

#[test]
fn an_archived_task_takes_no_event() {
    let d = Delivery::new();
    d.fx.service
        .control_plane()
        .command(
            &d.task,
            crate::orchestration::CommandRequest::new(
                crate::orchestration::OwnerCommand::Archive,
                "archive",
            ),
        )
        .unwrap();

    let refused = d.deliver(&d.task, "approved", json!({})).unwrap_err();

    assert_eq!(coded(&refused), "task_not_accepting_input");
    let record = d.fx.service.control_plane().task(&d.task).unwrap();
    assert_eq!(record.runner_state, RunnerState::Archived);
    assert!(record.event_inputs.is_empty());
    assert!(d.external_events().is_empty());
}

#[test]
fn a_wait_filter_compares_fields_flat_and_scalars_as_text() {
    let wait = |filter| EventWait {
        episode_id: "e".into(),
        event_type: "approved".into(),
        filter,
        since: chrono::Utc::now(),
        watch: None,
    };
    let data = json!({"mr": 17, "draft": false, "labels": ["a"], "by": "alice"});

    assert!(wait(json!(null)).matches("approved", &data));
    assert!(wait(json!({})).matches("approved", &data));
    assert!(wait(json!({"mr": "17", "draft": "false"})).matches("approved", &data));
    assert!(wait(json!({"mr": 17, "by": "alice"})).matches("approved", &data));
    assert!(wait(json!({"labels": ["a"]})).matches("approved", &data));
    assert!(!wait(json!({"mr": "17"})).matches("commented", &data));
    assert!(!wait(json!({"mr": "18"})).matches("approved", &data));
    assert!(!wait(json!({"missing": "x"})).matches("approved", &data));
    assert!(!wait(json!({"labels": "a"})).matches("approved", &data));
    assert!(!wait(json!("mr")).matches("approved", &data));
}

#[test]
fn a_matching_event_waits_on_the_server_and_resumes_a_task_whose_runner_is_down() {
    // D1: a task waiting in `wait` lost its slot (U4: `WaitingForEvent`
    // frees it) and stands `Stopped`, no runner polling. The event is
    // stored on the task record as a queued input, as an owner's choice of
    // a way out is, and the task is resumed once to take it.
    let d = Delivery::new();
    d.wait_for("approved", json!({}));
    {
        let control = d.fx.service.control_plane();
        let _lock = control.store().lock_task(&d.task).unwrap();
        let mut record = control.task(&d.task).unwrap();
        record.runner_state = RunnerState::Stopped;
        control.store().save_task(&mut record).unwrap();
    }

    // The fixture's resolver knows no backend; this one knows every one,
    // so the resume finds the task's backend available.
    let service = ProtocolService::new(
        ControlPlane::with_clock(StatePaths::new(&d.fx.root), d.fx.clock.clone()),
        Arc::new(AnyBackend),
    );
    let response = service
        .observer_deliver_event(
            &d.run,
            d.task.as_str(),
            ObserverEventRequest {
                event_type: "approved".into(),
                data: json!({"mr": 17}),
            },
        )
        .unwrap();

    assert_eq!(response.outcome, ObserverEffectOutcome::Delivered);
    let record = service.control_plane().task(&d.task).unwrap();
    assert_ne!(record.runner_state, RunnerState::Stopped);
    // The input waits on the record until the runner polls again.
    assert_eq!(record.event_inputs.len(), 1);
    let input = &record.event_inputs[0];
    assert_eq!(input.status, InputStatus::Queued);
    assert_eq!(response.input_id.as_deref(), Some(input.input_id.as_str()));
    let resume_key = format!("work-resume:{}", input.idempotency_key);
    let resumes = record
        .commands
        .iter()
        .filter(|receipt| receipt.idempotency_key == resume_key)
        .count();
    assert_eq!(resumes, 1);
}
