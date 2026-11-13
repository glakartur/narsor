//! The "deliver an event" effect on the service (R4, R13, R19, AE7): what
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
use crate::observers::{ObserverId, PlannedEffectKind, WatchMode};
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
        Self::with_dry_run(false)
    }

    fn with_dry_run(dry_run: bool) -> Self {
        let fx = Fixture::new();
        let observer = fx.active("mr-review", WatchMode::WatchList);
        let task = fx.task("task-1");
        fx.watch(&observer, &task, &format!("{PREFIX}a/-/merge_requests/17"));
        if dry_run {
            fx.service
                .request_observer_dry_run(&observer, Some(1), false)
                .unwrap();
        } else {
            fx.service.observer_pass();
        }
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
        key: &str,
        event_type: &str,
        data: serde_json::Value,
    ) -> Result<ObserverEffectResponse, ProtocolError> {
        self.fx.service.observer_deliver_event(
            &self.run,
            task.as_str(),
            ObserverEventRequest {
                effect_key: key.to_string(),
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
            deadline: None,
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

    let response = d
        .deliver(&d.task, "MR!17:approved", "approved", json!({"mr": 17}))
        .unwrap();

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
    // R19: the observer and the key the event came with.
    assert_eq!(provenance.observer_id, d.observer.to_string());
    assert_eq!(provenance.observer_name, "mr-review");
    assert_eq!(provenance.effect_key, "MR!17:approved");
    assert_eq!(provenance.run_id, d.run.to_string());
}

#[test]
fn a_matching_event_for_a_waiting_task_is_queued_for_its_step() {
    // The filter rendered from a template is text; the event's field a
    // number: they match as text.
    let d = Delivery::new();
    d.wait_for("approved", json!({"mr": "17"}));

    let response = d
        .deliver(
            &d.task,
            "MR!17:approved:episode-1",
            "approved",
            json!({"mr": 17, "by": "alice"}),
        )
        .unwrap();

    assert_eq!(response.outcome, ObserverEffectOutcome::Delivered);
    assert_eq!(response.input_id.as_deref(), Some("event-1"));
    let record = d.fx.service.control_plane().task(&d.task).unwrap();
    assert_eq!(record.event_inputs.len(), 1);
    let input = &record.event_inputs[0];
    assert_eq!(input.episode_id, "episode-1");
    assert_eq!(input.status, InputStatus::Queued);
    assert_eq!(input.data, Some(json!({"mr": 17, "by": "alice"})));
    assert_eq!(
        input.idempotency_key,
        format!("observer:{}:MR!17:approved:episode-1", d.observer)
    );
    assert_eq!(
        input.provenance.as_ref().map(|p| p.effect_key.as_str()),
        Some("MR!17:approved:episode-1")
    );
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

    let other_type = d
        .deliver(&d.task, "k-1", "commented", json!({"mr": 17}))
        .unwrap();
    let other_mr = d
        .deliver(&d.task, "k-2", "approved", json!({"mr": 18}))
        .unwrap();
    let no_field = d.deliver(&d.task, "k-3", "approved", json!({})).unwrap();

    for response in [&other_type, &other_mr, &no_field] {
        assert_eq!(response.outcome, ObserverEffectOutcome::Recorded);
    }
    let record = d.fx.service.control_plane().task(&d.task).unwrap();
    assert!(record.event_inputs.is_empty());
    assert_eq!(d.external_events().len(), 3);
}

#[test]
fn a_repeated_key_is_a_duplicate_and_records_nothing_twice() {
    let d = Delivery::new();
    d.wait_for("approved", json!({}));
    let first = d
        .deliver(&d.task, "MR!17:approved", "approved", json!({"mr": 17}))
        .unwrap();
    let again = d
        .deliver(&d.task, "MR!17:approved", "approved", json!({"mr": 99}))
        .unwrap();

    assert_eq!(first.outcome, ObserverEffectOutcome::Delivered);
    assert_eq!(again.outcome, ObserverEffectOutcome::Duplicate);
    assert_eq!(again.input_id.as_deref(), Some("event-1"));
    assert_eq!(d.external_events().len(), 1);
    let record = d.fx.service.control_plane().task(&d.task).unwrap();
    assert_eq!(record.event_inputs.len(), 1);
}

#[test]
fn two_concurrent_events_with_one_key_record_one() {
    let d = Delivery::new();
    let outcomes: Vec<ObserverEffectOutcome> = std::thread::scope(|scope| {
        let calls: Vec<_> = (0..2)
            .map(|_| {
                scope.spawn(|| {
                    d.deliver(&d.task, "MR!17:approved", "approved", json!({"mr": 17}))
                        .unwrap()
                        .outcome
                })
            })
            .collect();
        calls.into_iter().map(|call| call.join().unwrap()).collect()
    });

    let mut sorted: Vec<String> = outcomes.iter().map(|o| format!("{o:?}")).collect();
    sorted.sort();
    assert_eq!(sorted, ["Duplicate", "Recorded"]);
    assert_eq!(d.external_events().len(), 1);
}

#[test]
fn a_task_outside_the_watch_list_or_of_another_owner_is_refused() {
    let d = Delivery::new();
    let unwatched = d.fx.task("task-2");
    let refused = d
        .deliver(&unwatched, "k-1", "approved", json!({}))
        .unwrap_err();
    assert_eq!(coded(&refused), "task_not_watched");

    let foreign = task_of(&d.fx, "task-foreign", OwnerRef::new("someone-else"));
    d.fx.watch(
        &d.observer,
        &foreign,
        &format!("{PREFIX}a/-/merge_requests/19"),
    );
    let hidden = d
        .deliver(&foreign, "k-2", "approved", json!({}))
        .unwrap_err();
    assert!(matches!(hidden, ProtocolError::NotFound), "{hidden:?}");

    let bad_id =
        d.fx.service
            .observer_deliver_event(
                &d.run,
                "../task-1",
                ObserverEventRequest {
                    effect_key: "k-3".into(),
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
        .deliver(&d.task, "k-1", "review approved", json!({}))
        .unwrap_err();
    assert_eq!(coded(&spaced), "invalid_effect_data");
    let empty_key = d.deliver(&d.task, "", "approved", json!({})).unwrap_err();
    assert_eq!(coded(&empty_key), "invalid_effect_data");
    let huge = d
        .deliver(
            &d.task,
            "k-2",
            "approved",
            json!({"text": "x".repeat(super::MAX_EVENT_DATA_BYTES)}),
        )
        .unwrap_err();
    assert_eq!(coded(&huge), "invalid_effect_data");
    assert!(d.external_events().is_empty());
}

#[test]
fn a_dry_run_says_what_it_would_deliver_and_touches_no_task() {
    let d = Delivery::with_dry_run(true);
    d.wait_for("approved", json!({}));

    let would = d
        .deliver(&d.task, "MR!17:approved", "approved", json!({"mr": 17}))
        .unwrap();
    let again = d
        .deliver(&d.task, "MR!17:approved", "approved", json!({"mr": 17}))
        .unwrap();

    assert_eq!(would.outcome, ObserverEffectOutcome::WouldDeliver);
    assert_eq!(again.outcome, ObserverEffectOutcome::Duplicate);
    assert!(d.external_events().is_empty());
    let record = d.fx.service.control_plane().task(&d.task).unwrap();
    assert!(record.event_inputs.is_empty());
    assert!(record.external_event_keys.is_empty());
    let run = d.fx.service.observers().load_run(&d.run).unwrap();
    assert_eq!(run.planned_effects.len(), 1);
    assert_eq!(run.planned_effects[0].kind, PlannedEffectKind::DeliverEvent);
    assert_eq!(run.planned_effects[0].task_id.as_ref(), Some(&d.task));
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
        .observer_create_task(
            &events_run,
            ObserverCreateTaskRequest {
                effect_key: "k-1".into(),
                data: json!({}),
            },
        )
        .unwrap_err();
    assert_eq!(coded(&create), "effect_not_allowed");
    let event = fx
        .service
        .observer_deliver_event(
            &tasks_run,
            task.as_str(),
            ObserverEventRequest {
                effect_key: "k-1".into(),
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

    let refused = d
        .deliver(&d.task, "k-1", "approved", json!({}))
        .unwrap_err();

    assert_eq!(coded(&refused), "task_not_accepting_input");
    let record = d.fx.service.control_plane().task(&d.task).unwrap();
    assert_eq!(record.runner_state, RunnerState::Archived);
    assert!(record.external_event_keys.is_empty());
}

#[test]
fn a_wait_filter_compares_fields_flat_and_scalars_as_text() {
    let wait = |filter| EventWait {
        episode_id: "e".into(),
        event_type: "approved".into(),
        filter,
        since: chrono::Utc::now(),
        deadline: None,
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
fn a_matching_event_resumes_a_task_whose_slot_was_freed_while_it_waited() {
    // A task waiting for an event lost its slot (U4: `WaitingForEvent`
    // frees it) and stands `Stopped`: the event brings it back, once.
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
    let request = || ObserverEventRequest {
        effect_key: "MR!17:approved".into(),
        event_type: "approved".into(),
        data: json!({"mr": 17}),
    };

    let response = service
        .observer_deliver_event(&d.run, d.task.as_str(), request())
        .unwrap();
    let again = service
        .observer_deliver_event(&d.run, d.task.as_str(), request())
        .unwrap();

    assert_eq!(response.outcome, ObserverEffectOutcome::Delivered);
    assert_eq!(again.outcome, ObserverEffectOutcome::Duplicate);
    let record = service.control_plane().task(&d.task).unwrap();
    assert_ne!(record.runner_state, RunnerState::Stopped);
    let resume_key = format!("work-resume:observer:{}:MR!17:approved", d.observer);
    let resumes = record
        .commands
        .iter()
        .filter(|receipt| receipt.idempotency_key == resume_key)
        .count();
    assert_eq!(resumes, 1);
}
