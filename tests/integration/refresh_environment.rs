//! Refreshing the environment of an existing task (U4 of the environment
//! refresh): the owner route over HTTP in front of a real
//! `ProtocolService` over a seeded catalogue.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::{Duration, Utc};
use narsor_api_client::api::token::TokenSource;
use narsor_api_client::api::{ApiClient, ClientError};
use narsor_engine::controller_protocol::{
    CommandsQuery, ControllerIdentity, DesiredCommand, EnvironmentRefreshResult,
    OwnerCommandRequest, ProtocolError, ProtocolService, RefreshEnvironmentRequest,
    RefreshEnvironmentResponse,
};
use narsor_engine::domain::{BackendId, TaskId};
use narsor_engine::orchestration::{
    ControlPlane, ControllerReport, LeaseToken, Observation, ObservedSummary, OwnerCommand,
    OwnerRef, RunnerState, TaskEventKind, TaskRecord,
};
use narsor_engine::persistence::ServerStatePaths;
use narsor_protocol::controller::{
    CreateTaskRequest, EnvironmentDrift, LaunchRequest, TaskActionId, TaskSummary,
};
use narsor_server::api::{start_with_services, ApiHandle, ApiServices};

const TASK: &str = "task-refresh";
const BACKEND: &str = "local";
const CONTROLLER: &str = "controller-local";

/// A seeded catalogue with a local backend, a service over it and the
/// owner API on a loopback port.
struct Fixture {
    _root: TempRoot,
    state: PathBuf,
    service: Arc<ProtocolService>,
    server: ApiHandle,
    client: ApiClient,
}

/// A temporary directory, removed when dropped.
struct TempRoot(PathBuf);

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

impl Fixture {
    async fn new() -> Self {
        let root = std::env::temp_dir().join(format!("narsor-refresh-{}", uuid::Uuid::new_v4()));
        let state = root.join("state");
        let catalog = root.join("catalog");
        fs::create_dir_all(&state).unwrap();
        fs::create_dir_all(&catalog).unwrap();
        let resolver = crate::environment_support::seeded_resolver(&state);
        crate::environment_support::local_backend(&state, BACKEND);
        let service = Arc::new(ProtocolService::new(
            ControlPlane::new(ServerStatePaths::new(&state)),
            resolver,
        ));
        let server = start_with_services(
            catalog,
            ApiServices {
                control: Some(Arc::clone(&service)),
                ..ApiServices::default()
            },
        )
        .await
        .expect("start the server");
        let client = ApiClient::remote(
            &format!("http://{}", server.addr()),
            TokenSource::Static(server.token().to_string()),
        )
        .expect("a loopback client");
        Self {
            _root: TempRoot(root),
            state,
            service,
            server,
            client,
        }
    }

    /// Creates the task over HTTP while the catalogue has no task image,
    /// starts it through a controller and loses its workload: the task is
    /// `Interrupted`, sealed without a task image.
    async fn interrupted_task(&self) -> TaskRecord {
        let lease = self.started_task().await;
        self.report(&lease, Observation::WorkloadLost { state_known: true });
        let record = self.record();
        assert_eq!(record.runner_state, RunnerState::Interrupted);
        assert!(record.launch_spec.environment.task_image.is_none());
        record
    }

    /// Creates the task over HTTP and has a controller accept its start:
    /// the task holds its slot and runs. The lease comes back for later
    /// reports.
    async fn started_task(&self) -> LeaseToken {
        self.client
            .create_control_task(&CreateTaskRequest {
                task_id: Some(TASK.to_string()),
                backend_id: Some(BACKEND.to_string()),
                launch: LaunchRequest {
                    workflow_id: "wf".to_string(),
                    workflow_version: 1,
                    inputs: serde_json::json!({}),
                    workspaces: Default::default(),
                    profile: None,
                    harness: None,
                    resources: None,
                    ticket: None,
                    title: None,
                },
            })
            .await
            .expect("create the task");
        let lease = self
            .service
            .control_plane()
            .acquire_lease(&backend(), CONTROLLER, Duration::days(1))
            .expect("acquire lease")
            .token();
        self.service.reschedule(&backend());
        self.report(&lease, Observation::StartAccepted);
        lease
    }

    /// A report of the controller holding `lease` on the task at its
    /// current generation.
    fn report(&self, lease: &LeaseToken, observation: Observation) {
        self.service
            .control_plane()
            .report(ControllerReport {
                task_id: task_id(),
                lease: lease.clone(),
                desired_generation: self.record().generation,
                observation,
                resource: None,
                summary: ObservedSummary {
                    workload_present: true,
                    storage_present: true,
                    runner_status: None,
                    detail: None,
                },
                observed_at: Utc::now(),
            })
            .expect("controller report");
    }

    fn record(&self) -> TaskRecord {
        self.service.control_plane().task(&task_id()).unwrap()
    }

    /// Writes `runtime.yaml` with the seeded bundle and `task_image`.
    fn write_task_image(&self, task_image: &str) {
        fs::write(
            self.state.join("runtime.yaml"),
            format!(
                "bundle: ghcr.io/narsor/runtime@{}\ntask_image:\n{task_image}",
                crate::environment_support::digest('b')
            ),
        )
        .expect("write runtime.yaml");
    }

    /// The controller's desired command for the task, read as it polls.
    async fn desired_command(&self) -> DesiredCommand {
        let identity = ControllerIdentity {
            controller_id: CONTROLLER.to_string(),
            backend_id: backend(),
        };
        self.service
            .wait_for_commands(
                &identity,
                CommandsQuery {
                    wait_secs: Some(0),
                    since: None,
                },
            )
            .await
            .expect("desired commands")
            .commands
            .into_iter()
            .find(|command| command.task_id == task_id())
            .expect("a command for the task")
    }
}

fn task_id() -> TaskId {
    TaskId::parse(TASK).unwrap()
}

fn backend() -> BackendId {
    BackendId::parse(BACKEND).unwrap()
}

#[tokio::test]
async fn a_refresh_seals_the_new_task_image_for_the_next_start_and_names_the_authenticated_owner() {
    let fx = Fixture::new().await;
    let before = fx.interrupted_task().await;
    fx.write_task_image(
        "  base: debian:trixie-slim\n  components:\n    \
         - { name: glab, install: { inline: \"apt-get install -y glab\" } }\n",
    );

    // Another owner's task does not exist for them.
    let foreign = fx
        .service
        .refresh_environment(
            &OwnerRef::new("someone-else"),
            TASK,
            RefreshEnvironmentRequest::default(),
        )
        .unwrap_err();
    assert!(matches!(foreign, ProtocolError::NotFound), "{foreign:?}");

    // A requester named in the body is not the server's to believe.
    let response: RefreshEnvironmentResponse = reqwest::Client::new()
        .post(format!(
            "http://{}/control/v1/tasks/{TASK}/refresh-environment",
            fx.server.addr()
        ))
        .bearer_auth(fx.server.token())
        .json(&serde_json::json!({
            "expected_generation": before.generation,
            "requested_by": "someone-else",
        }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .expect("the refresh is accepted")
        .json()
        .await
        .unwrap();

    let after = fx.record();
    let EnvironmentRefreshResult::Refreshed {
        old_identity,
        new_identity,
        generation,
        change_count,
        diff,
    } = response.outcome
    else {
        panic!("the environment changed: {:?}", response.outcome);
    };
    assert_eq!(diff.task_image_components.added, vec!["glab".to_string()]);
    assert_eq!(change_count, diff.change_count());
    assert_eq!(old_identity, before.launch_spec.environment.identity);
    assert_eq!(new_identity, after.launch_spec.environment.identity);
    assert_eq!(generation, before.generation + 1);
    assert_eq!(response.task.generation, generation);
    assert_eq!(after.runner_state, RunnerState::Interrupted);
    assert!(after.launch_spec.environment.task_image.is_some());

    let refreshed_by: Vec<OwnerRef> = fx
        .client
        .control_events(TASK, before.revision)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|event| match event.kind {
            TaskEventKind::EnvironmentRefreshed { requested_by, .. } => Some(requested_by),
            _ => None,
        })
        .collect();
    assert_eq!(refreshed_by, vec![before.owner.clone()]);

    let command = fx.desired_command().await;
    assert_eq!(command.generation, generation);
    assert_eq!(command.launch_digest, after.launch_spec.digest);
    assert_ne!(command.launch_digest, before.launch_spec.digest);

    // The catalogue has not moved since: nothing changes.
    let again = fx
        .client
        .control_refresh_environment(TASK, &RefreshEnvironmentRequest::default())
        .await
        .unwrap();
    assert_eq!(
        again.outcome,
        EnvironmentRefreshResult::Unchanged {
            identity: new_identity
        }
    );
    assert_eq!(fx.record().revision, after.revision);
    fx.server.shutdown().await;
}

#[tokio::test]
async fn a_catalogue_that_does_not_resolve_refuses_the_refresh_and_the_task_resumes_as_it_was() {
    let fx = Fixture::new().await;
    let before = fx.interrupted_task().await;
    fx.write_task_image(
        "  base: debian:trixie-slim\n  components:\n    \
         - { name: glab, install: components/missing.sh }\n",
    );

    let error = fx
        .client
        .control_refresh_environment(
            TASK,
            &RefreshEnvironmentRequest {
                expected_generation: Some(before.generation),
            },
        )
        .await
        .unwrap_err();

    let ClientError::Request { code, message } = &error else {
        panic!("a client error naming the catalogue problem: {error:?}");
    };
    assert_eq!(code, "invalid_runtime", "{message}");
    assert!(
        message.contains("task_image.components[0].install"),
        "{message}"
    );
    let unchanged = fx.record();
    assert_eq!(
        (
            unchanged.generation,
            unchanged.revision,
            &unchanged.launch_spec.digest
        ),
        (
            before.generation,
            before.revision,
            &before.launch_spec.digest
        )
    );

    let resumed = fx
        .client
        .control_command(
            TASK,
            &OwnerCommandRequest {
                command: OwnerCommand::Resume,
                idempotency_key: "resume-1".to_string(),
                expected_generation: Some(before.generation),
            },
        )
        .await
        .expect("the task resumes with its old spec");
    assert_ne!(resumed.task.runner_state, RunnerState::Interrupted);
    assert_eq!(
        fx.desired_command().await.launch_digest,
        before.launch_spec.digest
    );
    fx.server.shutdown().await;
}

#[tokio::test]
async fn the_offer_and_the_refresh_refuse_a_running_task_and_allow_it_once_the_workload_is_gone() {
    let fx = Fixture::new().await;
    let lease = fx.started_task().await;
    let offers_refresh = |task: &TaskSummary| {
        task.actions
            .iter()
            .flatten()
            .any(|offer| offer.id == TaskActionId::RefreshEnvironment)
    };

    let running = fx.client.control_task(TASK).await.unwrap();
    assert!(!offers_refresh(&running), "{:?}", running.actions);
    let refused = fx
        .client
        .control_refresh_environment(
            TASK,
            &RefreshEnvironmentRequest {
                expected_generation: Some(running.generation),
            },
        )
        .await
        .unwrap_err();
    let ClientError::Refused { code, message } = &refused else {
        panic!("a refusal with its reason: {refused:?}");
    };
    assert_eq!(code, "action_unavailable");
    assert!(message.contains("wstrzymaj"), "{message}");

    fx.report(&lease, Observation::WorkloadLost { state_known: true });
    let interrupted = fx.client.control_task(TASK).await.unwrap();
    assert!(offers_refresh(&interrupted), "{:?}", interrupted.actions);
    let accepted = fx
        .client
        .control_refresh_environment(
            TASK,
            &RefreshEnvironmentRequest {
                expected_generation: Some(interrupted.generation),
            },
        )
        .await
        .expect("the offered refresh is accepted");
    assert!(matches!(
        accepted.outcome,
        EnvironmentRefreshResult::Unchanged { .. }
    ));
    fx.server.shutdown().await;
}

#[tokio::test]
async fn the_single_task_view_says_whether_the_environment_still_matches_the_catalogue() {
    let fx = Fixture::new().await;
    let stored = fx.interrupted_task().await;
    let drift = || async {
        fx.client
            .control_task_with_drift(TASK)
            .await
            .unwrap()
            .environment_drift
    };

    assert_eq!(drift().await, Some(EnvironmentDrift::Current));
    // Cost guard: only the single-task fetch that asks reads the catalogue.
    let listed = fx.client.control_tasks().await.unwrap();
    assert!(listed
        .tasks
        .iter()
        .all(|task| task.environment_drift.is_none()));
    assert_eq!(
        fx.client
            .control_task(TASK)
            .await
            .unwrap()
            .environment_drift,
        None
    );

    fx.write_task_image(
        "  base: debian:trixie-slim\n  components:\n    \
         - { name: glab, install: { inline: \"apt-get install -y glab\" } }\n",
    );
    match drift().await {
        Some(EnvironmentDrift::Differs { catalogue_identity }) => {
            assert_ne!(catalogue_identity, stored.launch_spec.environment.identity);
        }
        other => panic!("the catalogue added a component: {other:?}"),
    }

    fx.write_task_image(
        "  base: debian:trixie-slim\n  components:\n    \
         - { name: glab, install: components/missing.sh }\n",
    );
    match drift().await {
        Some(EnvironmentDrift::CatalogueProblem { code, reason }) => {
            assert_eq!(code, "invalid_runtime", "{reason}");
            assert!(
                reason.contains("task_image.components[0].install"),
                "{reason}"
            );
        }
        other => panic!("a broken runtime.yaml is a catalogue problem, not drift: {other:?}"),
    }
    fx.server.shutdown().await;
}

/// System-wide: a task refreshed and resumed through the real control plane
/// is accepted by the runner on the storage its first start initialized.
/// The runner's own spec fetch needs a running process, so the test serves
/// the spec as the controller reads it and drives the runner's check
/// ([`AcceptedReplacement::accept`]) and [`initialize`] on real storage.
#[tokio::test]
async fn a_refreshed_and_resumed_task_resumes_on_its_storage_with_the_new_spec() {
    use narsor_runner::runner::{
        initialize, AcceptedReplacement, Initialization, MountedSecrets, TaskStorage,
    };
    use narsor_runner::workspace::GitAccess;

    let fx = Fixture::new().await;
    let identity = ControllerIdentity {
        controller_id: CONTROLLER.to_string(),
        backend_id: backend(),
    };
    // The spec a runner receives travels as JSON.
    let served = |generation: u64| {
        let spec = fx
            .service
            .launch_spec(&identity, TASK, generation)
            .expect("the controller reads the spec");
        serde_json::from_value(serde_json::to_value(&spec).unwrap()).unwrap()
    };
    let initialize_with = |storage: &TaskStorage, spec, replacement| {
        initialize(
            storage,
            &storage.store(),
            &task_id(),
            spec,
            replacement,
            &MountedSecrets::new(None),
            &GitAccess::User,
        )
    };

    // The first start initializes the storage from the sealed spec.
    let lease = fx.started_task().await;
    let first = served(fx.record().generation);
    let storage = TaskStorage::new(fx.state.join("task-storage"));
    storage.prepare().unwrap();
    assert_eq!(
        initialize_with(&storage, &first, None).unwrap(),
        Initialization::Fresh
    );
    let marker = storage.marker().unwrap().expect("the marker");
    fx.report(&lease, Observation::WorkloadLost { state_known: true });

    // The owner refreshes with a catalogue that adds a component and resumes.
    fx.write_task_image(
        "  base: debian:trixie-slim\n  components:\n    \
         - { name: glab, install: { inline: \"apt-get install -y glab\" } }\n",
    );
    let refreshed = fx
        .client
        .control_refresh_environment(TASK, &RefreshEnvironmentRequest::default())
        .await
        .expect("the refresh is accepted");
    assert!(matches!(
        refreshed.outcome,
        EnvironmentRefreshResult::Refreshed { .. }
    ));
    fx.client
        .control_command(
            TASK,
            &OwnerCommandRequest {
                command: OwnerCommand::Resume,
                idempotency_key: "resume-refreshed".to_string(),
                expected_generation: Some(fx.record().generation),
            },
        )
        .await
        .expect("the task resumes");
    let command = fx.desired_command().await;
    let new = served(command.generation);
    assert_eq!(new.digest, command.launch_digest);
    assert!(new.environment.task_image.is_some());

    // The runner on the storage: its copy misses the new pin, and the spec
    // the control plane resealed is accepted as an environment refresh.
    let copy = storage
        .store()
        .load_task_launch_spec(&task_id())
        .unwrap()
        .expect("the storage's copy");
    assert!(copy.check_pinned_digest(&command.launch_digest).is_err());
    let replacement = AcceptedReplacement::accept(&copy, &new, &command.launch_digest)
        .expect("the resealed spec is an environment refresh of the copy");
    assert_eq!(
        initialize_with(&storage, &new, Some(&replacement)).unwrap(),
        Initialization::Resumed
    );

    assert_eq!(
        storage.store().load_task_launch_spec(&task_id()).unwrap(),
        Some(new.clone())
    );
    let moved = storage.marker().unwrap().expect("the marker");
    assert_eq!(moved, marker.for_launch(&new.digest));
    fx.server.shutdown().await;
}
