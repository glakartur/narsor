use crate::api::HumanInputDeliveryDto;
use chrono::Utc;

use super::{usage_window_length, HarnessUsageReport, UsageLimit, UsageStatus};
use super::{
    AttentionItem, AttentionKind, CapabilitiesView, ConductorReadQuery, ControllerListResponse,
    ControllerView, ConversationPageQuery, ConversationUnavailableReason, CreateEnrollmentRequest,
    CreateRepositoryRequest, CreateTaskRequest, DefaultBackendView, EnrollmentResponse,
    ExecutionEventsQuery, ExecutionEventsResponse, ExecutionEventsSource, Freshness, InputStatus,
    InputView, LaunchRequest, MoveTaskRequest, MoveTaskTarget, Observation, ObservationView,
    ObservedSummary, OperatorCommand, OperatorCommandStatus, OperatorCommandView, OwnerCommand,
    OwnerCommandRequest, QueueReason, QueueView, RepositoryListResponse, RepositoryView,
    ResourceIdentity, ResourceProfileRequest, RunnerState, SetDefaultBackendRequest,
    SubmitInputRequest, SubmitInputResponse, SubmitOperatorCommandRequest,
    SubmitOperatorCommandResponse, TaskDefaultHarness, TimelineResponse, ValidateWorkflowRequest,
    ValidateWorkflowResponse, WorkflowInputType, WorkflowInputView, WorkflowListResponse,
    WorkflowSource, WorkflowView, WorkspaceRequest,
};

#[test]
fn a_profile_step_is_sent_as_profile_and_an_unknown_kind_falls_back() {
    let profile = super::StepView {
        id: "profile".to_string(),
        label: "profile".to_string(),
        kind: super::StepKind::Profile,
    };
    let value = serde_json::to_value(&profile).unwrap();
    assert_eq!(value["kind"], "profile");
    assert_eq!(
        serde_json::from_value::<super::StepView>(value).unwrap(),
        profile
    );

    // A client that predates a kind reads it as `Unrecognized`, the way a
    // client older than `profile` reads a profile step.
    let newer = serde_json::json!({ "id": "x", "label": "x", "kind": "a_kind_from_later" });
    let step: super::StepView = serde_json::from_value(newer).unwrap();
    assert_eq!(step.kind, super::StepKind::Unrecognized);
}

#[test]
fn a_conductor_view_with_the_removed_step_profile_still_decodes() {
    let value = serde_json::json!({
        "available": true,
        "profile": [{ "step_id": "work", "unit": "1", "model": "large" }],
    });

    let view: super::ConductorView = serde_json::from_value(value).unwrap();

    assert!(view.available);
}

#[test]
fn entry_limit_attention_kind_round_trips_and_unknown_kinds_fall_back() {
    assert_eq!(
        serde_json::from_value::<AttentionKind>(serde_json::json!("entry_limit")).unwrap(),
        AttentionKind::EntryLimit
    );
    assert_eq!(
        serde_json::from_value::<AttentionKind>(serde_json::json!("future_kind")).unwrap(),
        AttentionKind::Unrecognized
    );
}

#[test]
fn task_summary_state_since_is_optional_on_the_wire() {
    let value = serde_json::json!({
        "task_id": "task-1",
        "backend_id": "local",
        "runner_state": "queued",
        "generation": 1,
        "revision": 1,
        "rank": 1,
        "created_at": "2026-10-03T12:00:00Z",
        "queue_sequence": 1,
        "workflow_id": "workflow",
        "workflow_version": 1,
        "launch_digest": "",
        "environment_identity": "",
        "archive": null,
        "observation": null,
        "runner": null,
        "controller": null,
        "inputs": [],
        "as_of": "2026-10-03T12:00:00Z"
    });
    let summary: super::TaskSummary = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(summary.state_since, None);
    assert!(serde_json::to_value(&summary)
        .unwrap()
        .get("state_since")
        .is_none());

    let since = chrono::DateTime::parse_from_rfc3339("2026-10-03T11:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let mut summary = summary;
    summary.state_since = Some(since);
    assert_eq!(
        serde_json::to_value(summary).unwrap()["state_since"],
        "2026-10-03T11:00:00Z"
    );
}

#[test]
fn task_event_keeps_the_durable_tagged_wire_shape() {
    let kind = super::TaskEventKind::RunnerReported {
        incarnation: "pod-1".into(),
        event_cursor: 7,
        execution_status: crate::api::ExecutionStatusDto::Running,
        pending_input_id: None,
        attention: Vec::new(),
    };
    let value = serde_json::to_value(&kind).unwrap();
    assert_eq!(value["type"], "runner_reported");
    assert_eq!(value["execution_status"], "running");
    assert_eq!(
        serde_json::from_value::<super::TaskEventKind>(value).unwrap(),
        kind
    );

    let unknown = serde_json::json!({ "type": "future_event" });
    assert_eq!(
        serde_json::from_value::<super::TaskEventKind>(unknown).unwrap(),
        super::TaskEventKind::Unrecognized
    );
}

#[test]
fn observer_events_carry_their_provenance_and_unknown_ones_with_fields_are_unrecognized() {
    let provenance = super::TaskProvenance {
        observer_id: "obs-1".into(),
        observer_name: "jira-new".into(),
        run_id: "obs-1.4".into(),
    };
    let kind = super::TaskEventKind::ExternalEventReceived {
        event_type: "review.approved".into(),
        source: "gitlab".into(),
        provenance: provenance.clone(),
        data: serde_json::json!({ "mr": 17 }),
        input_id: None,
    };
    let value = serde_json::to_value(&kind).unwrap();
    assert_eq!(value["type"], "external_event_received");
    assert_eq!(value["provenance"]["run_id"], "obs-1.4");
    assert!(value.get("input_id").is_none());
    assert_eq!(
        serde_json::from_value::<super::TaskEventKind>(value).unwrap(),
        kind
    );
    let created = super::TaskEventKind::CreatedByObserver { provenance };
    let value = serde_json::to_value(&created).unwrap();
    assert_eq!(value["type"], "created_by_observer");
    assert_eq!(
        serde_json::from_value::<super::TaskEventKind>(value).unwrap(),
        created
    );

    // What a client from before observers sees of a newer event: a known
    // fallback, not a parse error, even with fields it does not know.
    let newer = serde_json::json!({ "type": "future_event", "provenance": { "x": 1 } });
    assert_eq!(
        serde_json::from_value::<super::TaskEventKind>(newer).unwrap(),
        super::TaskEventKind::Unrecognized
    );
}

#[test]
fn observer_effect_requests_refuse_fields_a_script_may_not_choose() {
    let refused = serde_json::json!({
        "data": {},
        "workflow_id": "other",
    });
    assert!(serde_json::from_value::<super::ObserverCreateTaskRequest>(refused).is_err());
    let response = super::ObserverEffectResponse {
        outcome: super::ObserverEffectOutcome::Created,
        task_id: Some("task-1".into()),
        input_id: None,
        message: "created task task-1".into(),
    };
    let value = serde_json::to_value(&response).unwrap();
    assert_eq!(value["outcome"], "created");
    assert_eq!(
        serde_json::from_value::<super::ObserverEffectOutcome>(serde_json::json!("newer")).unwrap(),
        super::ObserverEffectOutcome::Unrecognized
    );
}

#[test]
fn a_worker_run_hides_its_token_and_an_unknown_outcome_is_unrecognized() {
    let run = super::ObserverWorkerRun {
        run_id: "mr-review~gitlab-mr-label~1".into(),
        phase: "check".into(),
        queue: "mr-review".into(),
        automation: "gitlab-mr-label".into(),
        token: "nso.mr-review~gitlab-mr-label~1.secret-part".into(),
        script_digest: "sha256:00".into(),
        script: "true".into(),
        input: serde_json::json!({}),
        secrets: Vec::new(),
        timeout_secs: 300,
        deadline: Utc::now(),
    };
    assert!(!format!("{run:?}").contains("secret-part"));
    assert_eq!(serde_json::to_value(&run).unwrap()["phase"], "check");

    let outcome: super::ObserverRunOutcome =
        serde_json::from_value(serde_json::json!({"kind": "exploded", "why": "newer"})).unwrap();
    assert_eq!(outcome, super::ObserverRunOutcome::Unrecognized);
    let too_large =
        serde_json::to_value(super::ObserverRunOutcome::OutputTooLarge { exit_code: 0 }).unwrap();
    assert_eq!(
        too_large,
        serde_json::json!({"kind": "output_too_large", "exit_code": 0})
    );
}

#[test]
fn owner_conversation_response_keeps_summary_only_wire_fields() {
    let now = Utc::now();
    let response = super::ConversationResponse::Available {
        attempts: vec![super::ConversationAttemptSummary {
            attempt_id: "attempt-1".into(),
            state_id: Some("review".into()),
            entries: 3,
            closed: true,
            last_entry_at: Some(now),
        }],
        observed_at: now,
        source: super::ExecutionEventsSource::Storage,
        recorded_at: Some(now),
    };
    let value = serde_json::to_value(&response).unwrap();
    assert_eq!(value["status"], "available");
    assert_eq!(value["source"], "storage");
    assert_eq!(value["attempts"][0]["entries"], 3);
    assert!(value["attempts"][0].get("content").is_none());
    assert_eq!(
        serde_json::from_value::<super::ConversationResponse>(value).unwrap(),
        response
    );
}

#[test]
fn runner_conversation_availability_keeps_its_tagged_shape() {
    let availability = super::ConversationAvailability::Unavailable {
        reason: "runner_unreachable".into(),
    };
    let value = serde_json::to_value(&availability).unwrap();
    assert_eq!(value["status"], "unavailable");
    assert_eq!(value["reason"], "runner_unreachable");
    assert_eq!(
        serde_json::from_value::<super::ConversationAvailability>(value).unwrap(),
        availability
    );
}

#[test]
fn unknown_conversation_reasons_keep_the_forward_compatible_fallback() {
    let reason: ConversationUnavailableReason =
        serde_json::from_str("\"new_server_reason\"").unwrap();
    assert_eq!(reason, ConversationUnavailableReason::Unrecognized);
}

#[test]
fn freshness_keeps_snake_case_wire_values() {
    assert_eq!(serde_json::to_string(&Freshness::Live).unwrap(), "\"live\"");
    assert_eq!(
        serde_json::to_string(&Freshness::Stale).unwrap(),
        "\"stale\""
    );
}

#[test]
fn timeline_response_uses_only_protocol_values() {
    let response = TimelineResponse {
        task_id: "task-123".into(),
        freshness: Freshness::Live,
        observed_at: None,
        timeline: None,
        execution_status: Some(crate::api::ExecutionStatusDto::Running),
        current_state: None,
        current_position: None,
    };
    let value = serde_json::to_value(&response).unwrap();

    assert_eq!(value["task_id"], "task-123");
    assert_eq!(value["freshness"], "live");
    assert_eq!(value["execution_status"], "running");
    assert_eq!(
        serde_json::from_value::<TimelineResponse>(value).unwrap(),
        response
    );
}

#[test]
fn queued_owner_responses_keep_their_wire_fields() {
    let input = SubmitInputResponse {
        input: InputView {
            input_id: "in-1".into(),
            pending_input_id: "wait-1".into(),
            status: InputStatus::Queued,
            submitted_at: Utc::now(),
            updated_at: Utc::now(),
            detail: None,
        },
        replayed: false,
    };
    let input_json = serde_json::to_value(input).unwrap();
    assert_eq!(input_json["input"]["input_id"], "in-1");
    assert_eq!(input_json["input"]["status"], "queued");
    assert_eq!(input_json["replayed"], false);

    let command = SubmitOperatorCommandResponse {
        command: OperatorCommandView {
            command_id: "cmd-1".into(),
            command: OperatorCommand::Interrupt,
            status: OperatorCommandStatus::Queued,
            submitted_at: Utc::now(),
            updated_at: Utc::now(),
            detail: None,
        },
        replayed: true,
    };
    let command_json = serde_json::to_value(command).unwrap();
    assert_eq!(command_json["command"]["command_id"], "cmd-1");
    assert_eq!(command_json["command"]["status"], "queued");
    assert_eq!(command_json["replayed"], true);
}

#[test]
fn owner_observation_and_queue_views_keep_their_shapes() {
    let observation = ObservationView {
        observation: Observation::RunnerReady,
        summary: ObservedSummary {
            workload_present: true,
            storage_present: true,
            runner_status: Some(crate::api::ExecutionStatusDto::Running),
            detail: None,
        },
        resource: Some(ResourceIdentity {
            uid: "pod-uid".into(),
            resource_generation: 4,
        }),
        observed_at: Utc::now(),
        received_at: Utc::now(),
        freshness: Freshness::Live,
    };
    let value = serde_json::to_value(observation).unwrap();
    assert_eq!(value["observation"]["kind"], "runner_ready");
    assert_eq!(value["summary"]["runner_status"], "running");
    assert_eq!(value["resource"]["uid"], "pod-uid");

    let queue = QueueView {
        reason: QueueReason::Admitted,
        message: "admitted; waiting for the controller to start it".into(),
        since: Utc::now(),
    };
    let value = serde_json::to_value(queue).unwrap();
    assert_eq!(value["reason"]["reason"], "admitted");
    assert_eq!(
        value["message"],
        "admitted; waiting for the controller to start it"
    );
}

#[test]
fn task_harness_keeps_its_tagged_wire_shape() {
    let value = serde_json::to_value(TaskDefaultHarness::Unavailable {
        harness: "codex".into(),
        reason: "not installed".into(),
    })
    .unwrap();

    assert_eq!(value["status"], "unavailable");
    assert_eq!(value["harness"], "codex");
    assert_eq!(value["reason"], "not installed");
}

#[test]
fn capabilities_keep_optional_reason_omitted_when_absent() {
    let value = serde_json::to_value(CapabilitiesView {
        connection_name: "local".into(),
        workflow_paths: true,
        workflow_paths_reason: None,
        local_repository_paths: true,
    })
    .unwrap();

    assert_eq!(value["connection_name"], "local");
    assert_eq!(value["workflow_paths"], true);
    assert!(value.get("workflow_paths_reason").is_none());
}

#[test]
fn default_backend_keeps_the_override_wire_name_and_value() {
    let value = serde_json::to_value(DefaultBackendView {
        configured: "local".into(),
        override_backend: Some("k8s".into()),
        effective: "k8s".into(),
        revision: 3,
        updated_at: None,
    })
    .unwrap();

    assert_eq!(value["override"], "k8s");
    assert!(value.get("override_backend").is_none());
    assert_eq!(
        serde_json::to_value(SetDefaultBackendRequest { backend_id: None }).unwrap(),
        serde_json::json!({ "backend_id": null })
    );
}

#[test]
fn workflow_input_keeps_the_wire_type_field_and_snake_case_value() {
    let value = serde_json::to_value(WorkflowInputView {
        name: "count".into(),
        input_type: WorkflowInputType::Integer,
        required: true,
        description: None,
        default: None,
    })
    .unwrap();

    assert_eq!(value["type"], "integer");
    assert_eq!(value["required"], true);
    assert!(value.get("input_type").is_none());
}

#[test]
fn repository_request_refuses_unknown_fields() {
    let value =
        serde_json::json!({"id":"repo","remote":"https://example.test/r.git","secret":"no"});
    assert!(serde_json::from_value::<CreateRepositoryRequest>(value).is_err());
}

#[test]
fn workflow_and_repository_views_keep_their_wire_names() {
    let workflow = WorkflowListResponse {
        workflows: vec![WorkflowView {
            name: "hello".into(),
            source: WorkflowSource::BuiltIn,
            description: None,
            version: Some(1),
            state_count: 1,
            initial_state: Some("start".into()),
            inputs: vec![WorkflowInputView {
                name: "count".into(),
                input_type: WorkflowInputType::Integer,
                required: true,
                description: None,
                default: None,
            }],
            workspaces: vec![],
            valid: true,
            violations: vec![],
            overrides: false,
            origin_label: "built-in".into(),
            shadow_labels: vec![],
            has_builtin: Some(true),
        }],
    };
    let repository = RepositoryListResponse {
        repositories: vec![RepositoryView {
            id: "repo".into(),
            name: "Repo".into(),
            provider: "github".into(),
            remote: "https://example.test/repo.git".into(),
            default_branch: None,
            description: None,
        }],
        unreadable: vec![],
    };

    let workflow_json = serde_json::to_value(workflow).unwrap();
    let repository_json = serde_json::to_value(repository).unwrap();
    assert_eq!(workflow_json["workflows"][0]["source"], "built_in");
    assert_eq!(workflow_json["workflows"][0]["state_count"], 1);
    assert_eq!(workflow_json["workflows"][0]["initial_state"], "start");
    assert_eq!(
        workflow_json["workflows"][0]["inputs"][0]["type"],
        "integer"
    );
    assert_eq!(repository_json["repositories"][0]["id"], "repo");
    assert_eq!(repository_json["unreadable"], serde_json::json!([]));
}

#[test]
fn a_legacy_workflow_view_marks_builtin_support_as_unknown() {
    let workflow: WorkflowView = serde_json::from_value(serde_json::json!({
        "name": "example",
        "source": "built_in",
        "inputs": [],
        "valid": true,
        "overrides": false
    }))
    .expect("legacy workflow view remains readable");

    assert_eq!(workflow.has_builtin, None);
}

#[test]
fn workflow_validation_keeps_the_path_request_and_result_fields() {
    let request = ValidateWorkflowRequest {
        path: "/workflows/task.yaml".into(),
    };
    let response = ValidateWorkflowResponse {
        valid: false,
        name: Some("task".into()),
        version: Some(2),
        state_count: 0,
        initial_state: None,
        inputs: vec![],
        workspaces: vec![],
        violations: vec!["missing state".into()],
    };

    let request = serde_json::to_value(request).unwrap();
    let response = serde_json::to_value(response).unwrap();
    assert_eq!(request["path"], "/workflows/task.yaml");
    assert_eq!(response["violations"][0], "missing state");
    assert_eq!(response["state_count"], 0);
}

#[test]
fn controller_enrollment_and_listing_use_string_backend_ids() {
    let enrollment = EnrollmentResponse {
        enrollment_code: "once".into(),
        backend_id: "k8s".into(),
        expires_at: Utc::now(),
    };
    let view = ControllerView {
        controller_id: "ctrl-1".into(),
        backend_id: "k8s".into(),
        name: None,
        enrolled_at: Utc::now(),
        revoked_at: None,
        last_heartbeat_at: None,
        holds_lease: false,
    };
    let value = serde_json::to_value(enrollment).unwrap();
    let list = serde_json::to_value(ControllerListResponse {
        controllers: vec![view],
    })
    .unwrap();

    assert_eq!(value["backend_id"], "k8s");
    assert_eq!(list["controllers"][0]["backend_id"], "k8s");
    assert_eq!(
        serde_json::to_value(CreateEnrollmentRequest::default()).unwrap(),
        serde_json::json!({ "name": null, "ttl_secs": null })
    );
}

#[test]
fn control_plane_queries_keep_their_default_wire_values() {
    assert_eq!(
        serde_json::to_value(ConversationPageQuery::default()).unwrap(),
        serde_json::json!({"from": 0, "byte_budget": null})
    );
    assert_eq!(
        serde_json::to_value(ExecutionEventsQuery::default()).unwrap(),
        serde_json::json!({"after": 0, "limit": null})
    );
    assert_eq!(
        serde_json::to_value(ConductorReadQuery::default()).unwrap(),
        serde_json::json!({"after": 0, "limit": null})
    );
    let events = ExecutionEventsResponse::Unavailable {
        reason: ConversationUnavailableReason::NotStarted,
        message: "runner has not started".into(),
    };
    assert_eq!(
        serde_json::to_value(events).unwrap(),
        serde_json::json!({
            "status": "unavailable",
            "reason": "not_started",
            "message": "runner has not started"
        })
    );
    let events = ExecutionEventsResponse::Available {
        events: vec![],
        has_more: false,
        source: ExecutionEventsSource::Storage,
    };
    assert_eq!(
        serde_json::to_value(events).unwrap(),
        serde_json::json!({
            "status": "available",
            "events": [],
            "has_more": false,
            "source": "storage"
        })
    );
}

#[test]
fn move_request_keeps_its_tagged_wire_shape_with_opaque_task_ids() {
    let request = MoveTaskRequest {
        target: MoveTaskTarget::Before("task-2".into()),
        expected_generation: Some(4),
    };
    let value = serde_json::to_value(&request).unwrap();
    assert_eq!(
        value,
        serde_json::json!({
            "target": {"to": "before", "task_id": "task-2"},
            "expected_generation": 4
        })
    );
    assert_eq!(
        serde_json::from_value::<MoveTaskRequest>(value).unwrap(),
        request
    );
}

#[test]
fn owner_request_values_keep_their_wire_shape() {
    let owner = OwnerCommandRequest {
        command: OwnerCommand::Pause,
        idempotency_key: "cmd-1".into(),
        expected_generation: Some(7),
    };
    assert_eq!(
        serde_json::to_value(owner).unwrap(),
        serde_json::json!({
            "command": "pause",
            "idempotency_key": "cmd-1",
            "expected_generation": 7
        })
    );

    let input = SubmitInputRequest {
        pending_input_id: "input-1".into(),
        idempotency_key: "answer-1".into(),
        delivery: HumanInputDeliveryDto {
            chosen_label: Some("yes".into()),
            ..HumanInputDeliveryDto::default()
        },
    };
    let value = serde_json::to_value(input).unwrap();
    assert_eq!(value["delivery"]["chosen_label"], "yes");
    assert_eq!(value["idempotency_key"], "answer-1");

    let command = SubmitOperatorCommandRequest {
        command: OperatorCommand::Transition {
            target: "done".into(),
            forced: true,
            reason: Some("operator decision".into()),
        },
        idempotency_key: "operator-1".into(),
        expected_generation: Some(8),
    };
    let value = serde_json::to_value(command).unwrap();
    assert_eq!(value["kind"], "transition");
    assert_eq!(value["override"], true);
    assert_eq!(value["reason"], "operator decision");
    assert_eq!(value["idempotency_key"], "operator-1");
}

#[test]
fn task_creation_contract_uses_only_protocol_values() {
    let request = CreateTaskRequest {
        task_id: Some("task-1".into()),
        backend_id: Some("local".into()),
        launch: LaunchRequest {
            workflow_id: "hello".into(),
            workflow_version: 2,
            inputs: serde_json::json!({"count": 3}),
            workspaces: std::collections::BTreeMap::from([(
                "code".to_string(),
                WorkspaceRequest {
                    repository: Some("repo".into()),
                    branch: Some("develop".into()),
                    commit: Some("abc123".into()),
                },
            )]),
            profile: None,
            harness: None,
            resources: Some(ResourceProfileRequest {
                cpu_millicores: 1500,
                memory_mib: 2048,
            }),
            ticket: None,
            title: None,
        },
    };
    let value = serde_json::to_value(request).unwrap();
    assert!(value["launch"].get("ticket").is_none());
    assert!(value["launch"].get("title").is_none());
    assert_eq!(value["launch"]["workflow_id"], "hello");
    assert_eq!(value["launch"]["workspaces"]["code"]["commit"], "abc123");
    assert_eq!(value["launch"]["workspaces"]["code"]["branch"], "develop");
    assert_eq!(value["launch"]["resources"]["cpu_millicores"], 1500);
    assert_eq!(value["launch"]["resources"]["memory_mib"], 2048);
}

#[test]
fn task_lifecycle_keeps_snake_case_states_and_archival_classification() {
    assert_eq!(
        serde_json::to_string(&RunnerState::PauseRequested).unwrap(),
        "\"pause_requested\""
    );
    assert!(!RunnerState::Stopped.is_archival());
    assert!(RunnerState::Archiving.is_archival());
    assert!(RunnerState::Archived.is_archival());
}

#[test]
fn orchestration_summary_values_keep_their_wire_shapes() {
    assert_eq!(
        serde_json::to_value(Observation::WorkloadLost { state_known: false }).unwrap(),
        serde_json::json!({"kind": "workload_lost", "state_known": false})
    );
    assert_eq!(
        serde_json::to_value(InputStatus::Obsolete).unwrap(),
        serde_json::json!("obsolete")
    );
    assert_eq!(
        serde_json::to_value(OperatorCommandStatus::Rejected).unwrap(),
        serde_json::json!("rejected")
    );
    assert_eq!(
        serde_json::to_value(QueueReason::WaitingForCapacity {
            position: 2,
            running: 3,
            limit: 4,
        })
        .unwrap(),
        serde_json::json!({
            "reason": "waiting_for_capacity",
            "position": 2,
            "running": 3,
            "limit": 4
        })
    );
    let attention = AttentionItem {
        kind: AttentionKind::Question,
        state_id: None,
        since: Utc::now(),
        waiting_id: "input-1".into(),
    };
    assert_eq!(
        serde_json::to_value(attention).unwrap()["waiting_id"],
        "input-1"
    );
}

#[test]
fn harness_usage_report_round_trips_unchanged() {
    let report = HarnessUsageReport {
        harness: "claude-code".into(),
        observed_at: "2026-10-31T12:00:00Z".parse().unwrap(),
        limits: vec![
            UsageLimit {
                window: "5h".into(),
                utilization: 0.01,
                resets_at: "2026-10-31T16:10:00Z".parse().unwrap(),
                status: Some(UsageStatus::Allowed),
            },
            UsageLimit {
                window: "7d".into(),
                utilization: 0.9,
                resets_at: "2026-11-02T15:00:00Z".parse().unwrap(),
                status: Some(UsageStatus::AllowedWarning),
            },
        ],
    };
    let value = serde_json::to_value(&report).unwrap();
    assert_eq!(value["limits"][1]["status"], "allowed_warning");
    assert_eq!(
        serde_json::from_value::<HarnessUsageReport>(value).unwrap(),
        report
    );
}

#[test]
fn a_usage_limit_without_status_leaves_it_out() {
    let limit: UsageLimit = serde_json::from_value(serde_json::json!({
        "window": "5h",
        "utilization": 0.5,
        "resets_at": "2026-10-31T16:10:00Z"
    }))
    .unwrap();
    assert_eq!(limit.status, None);
    assert!(serde_json::to_value(&limit)
        .unwrap()
        .get("status")
        .is_none());
}

#[test]
fn usage_statuses_keep_snake_case_and_unknown_ones_fall_back() {
    assert_eq!(
        serde_json::from_value::<UsageStatus>(serde_json::json!("allowed_warning")).unwrap(),
        UsageStatus::AllowedWarning
    );
    assert_eq!(
        serde_json::from_value::<UsageStatus>(serde_json::json!("something_new")).unwrap(),
        UsageStatus::Unrecognized
    );
}

#[test]
fn usage_windows_are_ordered_by_length() {
    let length = |window| usage_window_length(window).unwrap();
    assert!(length("5h") < length("7d"));
    assert!(length("90m") < length("5h"));
    assert_eq!(length("9999d"), chrono::Duration::days(9999));
}

#[test]
fn usage_windows_outside_the_pattern_have_no_length() {
    for window in [
        "", "h", "5", "5x", "0h", "05h", "12345h", "-5h", "+5h", "5H", "5h\n", "5 h", "５h",
    ] {
        assert_eq!(usage_window_length(window), None, "{window:?}");
    }
}

#[test]
fn an_observers_answer_names_its_entry_and_refuses_artifacts() {
    let request: super::ObserverDecideRequest = serde_json::from_value(serde_json::json!({
        "task_id": "task-1",
        "episode": "input-1",
        "choice": "check_feedback",
        "reason": "label removed",
    }))
    .unwrap();
    assert!(request.variables.is_empty());
    let value = serde_json::to_value(&request).unwrap();
    assert!(value.get("variables").is_none());
    assert_eq!(value["reason"], "label removed");

    let refused = serde_json::json!({
        "task_id": "task-1",
        "episode": "input-1",
        "choice": "check_feedback",
        "artifacts": {"report": "out.md"},
    });
    assert!(serde_json::from_value::<super::ObserverDecideRequest>(refused).is_err());

    // The input of an observer's answer names it; an owner's has nothing.
    let queued = super::TaskEventKind::InputQueued {
        input_id: "input-1".into(),
        pending_input_id: "episode-1".into(),
        observer: None,
    };
    let value = serde_json::to_value(&queued).unwrap();
    assert!(value.get("observer").is_none());
    assert_eq!(
        serde_json::from_value::<super::TaskEventKind>(value).unwrap(),
        queued
    );
}

#[test]
fn queue_views_round_trip_and_old_fields_are_optional() {
    let list = super::ObserverQueueListResponse {
        queues: vec![
            super::ObserverQueueSummary {
                queue: "mr-review".into(),
                mode: Some("on_episodes".into()),
                interval_secs: None,
                cron: Some("*/5 * * * *".into()),
                automations: vec!["gitlab-mr-label".into()],
                entries: 1,
                problem: None,
            },
            super::ObserverQueueSummary {
                queue: "broken".into(),
                mode: None,
                interval_secs: None,
                cron: None,
                automations: Vec::new(),
                entries: 0,
                problem: Some("automation 'missing' is not in the catalogue".into()),
            },
        ],
        problems: vec![super::CatalogProblemView {
            file: "queues/broken.yaml".into(),
            reason: "automation 'missing' is not in the catalogue".into(),
        }],
    };
    let value = serde_json::to_value(&list).unwrap();
    assert!(value["queues"][1].get("mode").is_none());
    assert_eq!(
        serde_json::from_value::<super::ObserverQueueListResponse>(value).unwrap(),
        list
    );
    // A server from before the catalogue sends no problems and no
    // automations: a new client still reads it.
    let old: super::ObserverQueueListResponse = serde_json::from_value(serde_json::json!({
        "queues": [{"queue": "q", "observers": 1, "entries": 0}]
    }))
    .unwrap();
    assert!(old.problems.is_empty());
    assert!(old.queues[0].automations.is_empty());

    let now = Utc::now();
    let queue = super::ObserverQueueView {
        queue: "mr-review".into(),
        mode: Some("on_episodes".into()),
        interval_secs: None,
        cron: Some("*/5 * * * *".into()),
        workflow_id: None,
        automations: vec![super::QueueAutomationView {
            automation: "gitlab-mr-label".into(),
            description: "label".into(),
            scripts: vec!["in".into(), "check".into(), "out".into()],
            secrets: Vec::new(),
            state: "active".into(),
            next_run_at: Some(now),
            waiting_for_claim_since: None,
            worker_missing: false,
            last_claim_at: None,
            active_run: None,
            last_run: None,
            consecutive_failures: 0,
            errors: Vec::new(),
        }],
        entries: vec![super::QueueEntryView {
            entry_id: "a".repeat(64),
            task_id: "task-1".into(),
            step: "review_work".into(),
            episode: "input-1".into(),
            started_at: now,
            params: [("mr_url".to_string(), "https://x/1".to_string())].into(),
            automations: vec![super::QueueEntryAutomationView {
                automation: "gitlab-mr-label".into(),
                running: false,
                last_run_at: Some(now),
                last_error: Some(super::QueueEntryErrorView {
                    run_id: "mr-review~gitlab-mr-label~4".into(),
                    status: "failed".into(),
                    finished_at: now,
                    exit_code: Some(2),
                    error: "the script exited with code 2".into(),
                }),
            }],
            gate_phase: Some("ready".into()),
        }],
        problems: Vec::new(),
    };
    let value = serde_json::to_value(&queue).unwrap();
    assert_eq!(
        value["entries"][0]["automations"][0]["last_error"]["exit_code"],
        2
    );
    assert!(value["automations"][0].get("secrets").is_none());
    assert_eq!(
        serde_json::from_value::<super::ObserverQueueView>(value).unwrap(),
        queue
    );
}

/// R1: a command names the spec by its digest; the spec itself is fetched
/// on demand and never rides along.
#[test]
fn a_command_carries_the_launch_digest_and_no_launch_spec() {
    let response = super::CommandsResponse {
        lease_epoch: 1,
        cursor: 7,
        full: true,
        commands: vec![super::DesiredCommand {
            task_id: crate::identifier::TaskId::parse("task-1").unwrap(),
            generation: 2,
            runner_state: RunnerState::Queued,
            action: super::DesiredAction::Start,
            resource_key: "task-1".into(),
            launch_digest: "sha256:abc".into(),
            runner: None,
            reader_needed: false,
        }],
    };

    let value = serde_json::to_value(&response).unwrap();

    let command = &value["commands"][0];
    assert!(command.get("launch_spec").is_none(), "{command}");
    assert_eq!(command["launch_digest"], "sha256:abc");
}
