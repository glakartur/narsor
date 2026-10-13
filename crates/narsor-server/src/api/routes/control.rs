//! The owner routes of `/control/v1` that read the daemon's own disk (U6):
//! the workflow catalogue, workflow validation, the repository registry,
//! the Historia of executions from before the task scheduler, and the
//! capabilities a client checks before it offers any of it.
//!
//! They sit beside [`narsor_engine::controller_protocol::http::owner_routes`], behind
//! the same owner gate, and exist only where the control plane does. What
//! reads files of the server (a workflow *path*, the old executions) is
//! served to the trusted local daemon alone: a network-mode server answers
//! that it does not, and says so in its capabilities, so a client never
//! has to find out by failing.
//!
//! Text that comes from files or from runners and that a client only
//! displays is cleaned of control and invisible format characters
//! ([`without_unsafe_chars`]); nothing here returns a path of the server.

use std::path::Path as FsPath;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use axum::{Json, Router};

use super::{
    begin_recovery, build_summary, is_safe_id, read_conductor_chat, read_conductor_state,
    read_conversation, read_events, read_timeline, ApiError, AppState, ConversationQuery,
    EventsQuery,
};
use narsor_engine::api::dto::{
    AcceptedResponse, ActiveProblem, ConductorChatResponse, ConductorStateResponse, RecoveryRequest,
};
use narsor_engine::controller_protocol::{
    CapabilitiesView, CreateRepositoryRequest, LegacyExecutionListResponse, LegacyExecutionView,
    ProtocolError, RepositoryListResponse, RepositoryView, ValidateWorkflowRequest,
    ValidateWorkflowResponse, WorkflowInputType, WorkflowInputView, WorkflowListResponse,
    WorkflowSource, WorkflowSourceRequest, WorkflowSourceResponse, WorkflowView,
};
use narsor_engine::domain::display::{
    without_unsafe_chars, without_unsafe_chars_json, without_unsafe_chars_multiline,
};
use narsor_engine::domain::{Event, ExecutionStatus, ParamType, WorkflowDefinition};
use narsor_engine::engine::Timeline;
use narsor_engine::persistence::ExecutionStore;
use narsor_engine::registry::{RegistryError, RepositoryEntry, RepositoryRegistry};
use narsor_engine::tasks::{self, TaskError};
use narsor_engine::workflows::{WorkflowEntry, WorkflowOrigin, WorkflowResolver};

/// The largest workflow file validation reads. A definition is a few
/// kilobytes; anything near this is not one, and the file is the server's
/// disk read on a client's say-so.
const MAX_WORKFLOW_FILE_BYTES: u64 = 1024 * 1024;

/// Why a network-mode server does not read workflow files.
const WORKFLOW_PATHS_REASON: &str =
    "this server is not a trusted local daemon: it does not read workflow files from its disk";

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/control/v1/capabilities", get(get_capabilities))
        .route("/control/v1/workflows", get(get_workflows))
        .route("/control/v1/workflows/source", post(post_workflow_source))
        .route(
            "/control/v1/workflows/validate",
            post(post_validate_workflow),
        )
        .route(
            "/control/v1/repositories",
            get(get_repositories).post(post_repository),
        )
        .route("/control/v1/repositories/:id", delete(delete_repository))
        .route("/control/v1/executions", get(get_history))
        .route("/control/v1/executions/:execution_id", get(get_history_one))
        .route(
            "/control/v1/executions/:execution_id/events",
            get(get_history_events),
        )
        .route(
            "/control/v1/executions/:execution_id/timeline",
            get(get_history_timeline),
        )
        .route(
            "/control/v1/executions/:execution_id/attempts/:attempt_id/conversation",
            get(get_history_conversation),
        )
        .route(
            "/control/v1/executions/:execution_id/conductor",
            get(get_history_conductor),
        )
        .route(
            "/control/v1/executions/:execution_id/conductor/chat",
            get(get_history_conductor_chat),
        )
        .route(
            "/control/v1/executions/:execution_id/recover",
            post(post_history_recover),
        )
}

/// Runs blocking file work off the async threads.
async fn blocking<T, F>(work: F) -> Result<T, ProtocolError>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| ProtocolError::Internal(error.to_string()))
}

// ---------------------------------------------------------------------------
// Capabilities
// ---------------------------------------------------------------------------

/// `GET /control/v1/capabilities`.
async fn get_capabilities(
    State(state): State<AppState>,
) -> Result<Json<CapabilitiesView>, ApiError> {
    let legacy_executions = state.local && !state.store.list_execution_ids()?.is_empty();
    Ok(Json(CapabilitiesView {
        connection_name: without_unsafe_chars(&state.connection_name),
        workflow_paths: state.local,
        workflow_paths_reason: (!state.local).then(|| WORKFLOW_PATHS_REASON.to_string()),
        legacy_executions,
        local_repository_paths: state.local,
    }))
}

// ---------------------------------------------------------------------------
// Workflows
// ---------------------------------------------------------------------------

/// `GET /control/v1/workflows` — built-in and catalogue workflows.
async fn get_workflows(
    State(state): State<AppState>,
) -> Result<Json<WorkflowListResponse>, ApiError> {
    let resolver = state.workflows.clone();
    let workflows = blocking(move || {
        resolver
            .list()
            .into_iter()
            .map(|entry| workflow_view(&resolver, entry))
            .collect()
    })
    .await?;
    Ok(Json(WorkflowListResponse { workflows }))
}

/// Returns source only for an explicit show/eject request, never for every
/// row in the workflow catalogue.
async fn post_workflow_source(
    State(state): State<AppState>,
    Json(request): Json<WorkflowSourceRequest>,
) -> Result<Json<WorkflowSourceResponse>, ApiError> {
    let resolver = state.workflows.clone();
    let local = state.local;
    Ok(blocking(move || {
        if request.builtin {
            let name = narsor_engine::workflows::workflow_name(&request.name);
            let builtin = narsor_engine::workflows::builtin(name).ok_or(ProtocolError::NotFound)?;
            return Ok(Json(WorkflowSourceResponse {
                name: builtin.name.to_string(),
                origin_label: "built-in".to_string(),
                content: builtin.source.to_string(),
            }));
        }
        if !local {
            return Err(ProtocolError::bad_request(
                "workflow_source_unavailable",
                WORKFLOW_PATHS_REASON,
            ));
        }
        let resolved = resolver
            .resolve_name(&request.name)
            .map_err(|_| ProtocolError::NotFound)?;
        Ok(Json(WorkflowSourceResponse {
            name: resolved.name,
            origin_label: resolved.origin.label().to_string(),
            content: resolved.source,
        }))
    })
    .await??)
}

/// One catalogue entry with what a form needs of its definition. A
/// definition that does not load stays listed, with its violations, so the
/// operator sees *why* it cannot be chosen.
fn workflow_view(resolver: &WorkflowResolver, entry: WorkflowEntry) -> WorkflowView {
    let resolved = resolver.resolve_name(&entry.name);
    let loaded = resolved
        .as_ref()
        .map_err(|_| vec!["the workflow could not be read".to_string()])
        .and_then(|resolved| tasks::load_resolved(resolved).map_err(violations_of));
    let (version, state_count, initial_state, inputs, violations) = match loaded {
        Ok(definition) => (
            Some(definition.version),
            definition.states.len(),
            Some(definition.initial_state.clone()),
            inputs_of(&definition),
            Vec::new(),
        ),
        Err(violations) => (None, 0, None, Vec::new(), violations),
    };
    let overrides = entry.is_override();
    let origin_label = entry.origin.label().to_string();
    let shadow_labels = entry
        .shadows
        .iter()
        .map(|origin| origin.label().to_string())
        .collect();
    WorkflowView {
        name: without_unsafe_chars(&entry.name),
        source: match entry.origin {
            WorkflowOrigin::BuiltIn => WorkflowSource::BuiltIn,
            WorkflowOrigin::User(_) => WorkflowSource::User,
            WorkflowOrigin::Root(_) | WorkflowOrigin::File(_) => WorkflowSource::Catalog,
        },
        description: entry
            .description
            .map(|description| without_unsafe_chars(&description)),
        valid: violations.is_empty(),
        overrides,
        version,
        state_count,
        initial_state,
        inputs,
        violations,
        origin_label,
        shadow_labels,
        has_builtin: Some(narsor_engine::workflows::builtin(&entry.name).is_some()),
    }
}

/// The declared inputs, cleaned for display.
fn inputs_of(definition: &WorkflowDefinition) -> Vec<WorkflowInputView> {
    definition
        .inputs
        .iter()
        .map(|input| WorkflowInputView {
            name: without_unsafe_chars(&input.name),
            input_type: match input.param_type {
                ParamType::String => WorkflowInputType::String,
                ParamType::Text => WorkflowInputType::Text,
                ParamType::Integer => WorkflowInputType::Integer,
                ParamType::Boolean => WorkflowInputType::Boolean,
                ParamType::Repository => WorkflowInputType::Repository,
            },
            required: input.required,
            description: input
                .description
                .as_deref()
                .map(without_unsafe_chars_multiline),
            default: input.default.clone().map(|mut default| {
                without_unsafe_chars_json(&mut default);
                default
            }),
        })
        .collect()
}

/// The violations a failed load stands for, cleaned for display.
fn violations_of(error: TaskError) -> Vec<String> {
    let violations = match error {
        TaskError::InvalidDefinition { violations } => violations,
        other => vec![other.to_string()],
    };
    violations
        .iter()
        .map(|violation| without_unsafe_chars_multiline(violation))
        .collect()
}

/// `POST /control/v1/workflows/validate` — every violation of a workflow
/// file on the server's disk, like `narsor validate`.
///
/// Reading a file named by the caller is the one thing here a network-mode
/// server must never do, so the capability is checked first; the file must
/// be a regular `.yaml`/`.yml` file of a sane size, and nothing of it but
/// the verdict is returned.
async fn post_validate_workflow(
    State(state): State<AppState>,
    Json(request): Json<ValidateWorkflowRequest>,
) -> Result<Json<ValidateWorkflowResponse>, ApiError> {
    if !state.local {
        return Err(ProtocolError::bad_request(
            "workflow_paths_unavailable",
            WORKFLOW_PATHS_REASON,
        )
        .into());
    }
    Ok(
        blocking(move || validate_workflow_file(FsPath::new(&request.path)))
            .await?
            .map(Json)?,
    )
}

fn validate_workflow_file(path: &FsPath) -> Result<ValidateWorkflowResponse, ProtocolError> {
    let unusable = |code: &'static str, message: &str| ProtocolError::bad_request(code, message);
    let has_yaml_extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| narsor_engine::workflows::EXTENSIONS.contains(&extension));
    if !has_yaml_extension {
        return Err(unusable(
            "workflow_not_yaml",
            "a workflow file ends in .yaml or .yml",
        ));
    }
    let metadata = std::fs::metadata(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            unusable(
                "workflow_not_found",
                "there is no workflow file at that path",
            )
        } else {
            unusable("workflow_unreadable", "the workflow file cannot be read")
        }
    })?;
    if !metadata.is_file() {
        return Err(unusable(
            "workflow_not_a_file",
            "that path is not a regular file",
        ));
    }
    if metadata.len() > MAX_WORKFLOW_FILE_BYTES {
        return Err(unusable(
            "workflow_too_large",
            "the file is too large to be a workflow definition",
        ));
    }
    let source = std::fs::read_to_string(path).map_err(|_| {
        unusable(
            "workflow_unreadable",
            "the workflow file cannot be read as text",
        )
    })?;
    Ok(verdict_on(&source))
}

/// The verdict on workflow source: all violations at once, or the reason
/// the text cannot be read as a definition at all.
fn verdict_on(source: &str) -> ValidateWorkflowResponse {
    let refused = |violations: Vec<String>| ValidateWorkflowResponse {
        valid: false,
        name: None,
        version: None,
        state_count: 0,
        initial_state: None,
        inputs: Vec::new(),
        violations,
    };
    let clean = |text: String| without_unsafe_chars_multiline(&text);
    match narsor_engine::workflow::validate_source(source) {
        Err(error) => refused(vec![clean(error.to_string())]),
        Ok(violations) if !violations.is_empty() => {
            refused(violations.iter().map(|v| clean(v.to_string())).collect())
        }
        Ok(_) => match narsor_engine::workflow::load_from_str(source) {
            Ok(definition) => ValidateWorkflowResponse {
                valid: true,
                name: Some(without_unsafe_chars(&definition.name)),
                version: Some(definition.version),
                state_count: definition.states.len(),
                initial_state: Some(without_unsafe_chars(&definition.initial_state)),
                inputs: inputs_of(&definition),
                violations: Vec::new(),
            },
            Err(error) => refused(vec![clean(error.to_string())]),
        },
    }
}

// ---------------------------------------------------------------------------
// Repositories
// ---------------------------------------------------------------------------

/// `GET /control/v1/repositories`.
async fn get_repositories(
    State(state): State<AppState>,
) -> Result<Json<RepositoryListResponse>, ApiError> {
    let registry = RepositoryRegistry::new(&state.catalog_dir);
    let listing = blocking(move || registry.list())
        .await?
        .map_err(registry_refusal)?;
    Ok(Json(RepositoryListResponse {
        repositories: listing.entries.iter().map(repository_view).collect(),
        unreadable: listing
            .failures
            .iter()
            .map(|failure| {
                let file = failure
                    .path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                without_unsafe_chars(&format!("{file}: {}", failure.reason))
            })
            .collect(),
    }))
}

/// `POST /control/v1/repositories` — registers one, validated exactly as
/// `narsor repo add` does it.
async fn post_repository(
    State(state): State<AppState>,
    Json(request): Json<CreateRepositoryRequest>,
) -> Result<(StatusCode, Json<RepositoryView>), ApiError> {
    for (field, text) in [("id", Some(&request.id)), ("remote", Some(&request.remote))] {
        if text.is_some_and(|text| *text != without_unsafe_chars(text)) {
            return Err(ProtocolError::bad_request(
                "invalid_repository",
                format!("the {field} contains control characters"),
            )
            .into());
        }
    }
    if !state.local && !is_remote_git_url(&request.remote) {
        return Err(ProtocolError::bad_request(
            "invalid_repository",
            "this server is not a trusted local daemon: a repository must be a remote git URL \
             (https, ssh or user@host:path), not a path on the server's disk",
        )
        .into());
    }
    let clean = |text: Option<String>| text.map(|text| without_unsafe_chars(&text));
    let entry = RepositoryEntry::described(
        &request.id,
        &request.remote,
        clean(request.name),
        clean(request.provider),
        clean(request.default_branch),
        clean(request.description),
    );
    let registry = RepositoryRegistry::new(&state.catalog_dir);
    let view = repository_view(&entry);
    blocking(move || registry.add(entry))
        .await?
        .map_err(registry_refusal)?;
    Ok((StatusCode::CREATED, Json(view)))
}

/// `DELETE /control/v1/repositories/:id` — answers the entry it removed;
/// an id nobody registered is `404`.
async fn delete_repository(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<RepositoryView>, ApiError> {
    let registry = RepositoryRegistry::new(&state.catalog_dir);
    let removed = blocking(move || registry.remove(&id))
        .await?
        .map_err(|error| match error {
            // An id that cannot name an entry names none.
            RegistryError::InvalidId { .. } => ProtocolError::NotFound,
            other => registry_refusal(other),
        })?;
    Ok(Json(repository_view(&removed)))
}

fn repository_view(entry: &RepositoryEntry) -> RepositoryView {
    RepositoryView {
        id: without_unsafe_chars(&entry.id),
        name: without_unsafe_chars(&entry.name),
        provider: without_unsafe_chars(&entry.provider),
        remote: without_unsafe_chars(&without_userinfo(&entry.remote)),
        default_branch: entry.default_branch.as_deref().map(without_unsafe_chars),
        description: entry
            .description
            .as_deref()
            .map(without_unsafe_chars_multiline),
    }
}

/// Whether `remote` names a git server rather than the server's own disk:
/// an `https`/`ssh`/`git` URL or an scp-like `[user@]host:path`.
fn is_remote_git_url(remote: &str) -> bool {
    // Checked as given, not trimmed: what is validated is what gets stored.
    // Whitespace, control characters and backslashes are where parsers
    // disagree about where a URL ends.
    if remote.is_empty()
        || remote.chars().any(|ch| ch.is_whitespace() || ch == '\\')
        || without_unsafe_chars(remote) != remote
    {
        return false;
    }
    // `ext::cmd` and `fd::n` are git transports that run commands, and
    // `file:` is the server's own disk; none is a git server.
    if remote.contains("::") {
        return false;
    }
    if let Some((scheme, rest)) = remote.split_once("://") {
        let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
        let host = authority.rsplit('@').next().unwrap_or_default();
        return matches!(
            scheme.to_ascii_lowercase().as_str(),
            "https" | "ssh" | "git"
        ) && !host.is_empty()
            && !host.starts_with(['-', ':']);
    }
    if remote.starts_with(['/', '.', '~', '-']) {
        return false;
    }
    // scp-like: a host, then a colon before any slash.
    remote.split_once(':').is_some_and(|(host, path)| {
        !host.is_empty()
            && !host.contains('/')
            && !path.is_empty()
            && !host.eq_ignore_ascii_case("file")
            && !host.rsplit('@').next().unwrap_or_default().starts_with('-')
    })
}

/// A remote URL without the credentials it may embed
/// (`https://user:token@host/…`): a client shows it, it never needs them.
fn without_userinfo(remote: &str) -> String {
    let Some((scheme, rest)) = remote.split_once("://") else {
        return remote.to_string();
    };
    let (authority, path) = rest
        .find(['/', '?', '#'])
        .map_or((rest, ""), |at| rest.split_at(at));
    match authority.rsplit_once('@') {
        Some((_, host)) => format!("{scheme}://{host}{path}"),
        None => remote.to_string(),
    }
}

/// What a refused registry operation is to the caller.
fn registry_refusal(error: RegistryError) -> ProtocolError {
    match error {
        RegistryError::DuplicateId { id, .. } => ProtocolError::conflict(
            "repository_exists",
            format!("repository '{id}' is already registered"),
        ),
        RegistryError::NotFound { .. } => ProtocolError::NotFound,
        RegistryError::InvalidId { .. }
        | RegistryError::InvalidRemote { .. }
        | RegistryError::InvalidBaseImage { .. }
        | RegistryError::InvalidGitCredential { .. } => ProtocolError::bad_request(
            "invalid_repository",
            without_unsafe_chars_multiline(&error.to_string()),
        ),
        other => ProtocolError::Internal(other.to_string()),
    }
}

// ---------------------------------------------------------------------------
// History: executions from before the task scheduler
// ---------------------------------------------------------------------------

/// The store of a legacy execution, for the trusted local daemon only: any
/// other server, and any id it does not hold, answers `404`.
fn legacy_store(state: &AppState, execution_id: &str) -> Result<ExecutionStore, ApiError> {
    if state.local && is_safe_id(execution_id) && state.store.exists(execution_id) {
        Ok(state.store.clone())
    } else {
        Err(ApiError::ExecutionNotFound)
    }
}

/// `GET /control/v1/executions` — newest first; empty on a server that
/// holds none.
async fn get_history(
    State(state): State<AppState>,
) -> Result<Json<LegacyExecutionListResponse>, ApiError> {
    let executions = blocking(move || -> Result<_, ApiError> {
        let mut executions = Vec::new();
        if state.local {
            for id in state.store.list_execution_ids()? {
                // One corrupt execution must not take the whole Historia away.
                match history_view(&state, &id) {
                    Ok(view) => executions.push(view),
                    Err(_) => {
                        tracing::warn!(execution_id = %id, "control: skipping an unreadable execution")
                    }
                }
            }
        }
        executions.sort_by(|a, b| {
            b.started_at
                .cmp(&a.started_at)
                .then_with(|| b.id.cmp(&a.id))
        });
        Ok(executions)
    })
    .await
    .map_err(|error| ApiError::Internal(error.to_string()))??;
    Ok(Json(LegacyExecutionListResponse { executions }))
}

/// `GET /control/v1/executions/:execution_id`.
async fn get_history_one(
    State(state): State<AppState>,
    Path(execution_id): Path<String>,
) -> Result<Json<LegacyExecutionView>, ApiError> {
    legacy_store(&state, &execution_id)?;
    Ok(Json(history_view(&state, &execution_id)?))
}

fn history_view(state: &AppState, execution_id: &str) -> Result<LegacyExecutionView, ApiError> {
    let execution = state.store.load_execution(execution_id)?;
    let summary = build_summary(&state.store, &execution)?;
    let status: ExecutionStatus = summary.status;
    let not_recoverable_reason = match &status {
        _ if state.engine.is_active(execution_id) => {
            Some("it is being processed right now".to_string())
        }
        ExecutionStatus::Failed | ExecutionStatus::Unknown => None,
        other => Some(format!(
            "a {} execution cannot be recovered; only a failed one can",
            other.as_str()
        )),
    };
    Ok(LegacyExecutionView {
        id: summary.id,
        workflow_id: without_unsafe_chars(&summary.workflow_id),
        workflow_version: summary.workflow_version,
        status,
        current_state: summary.current_state.as_deref().map(without_unsafe_chars),
        started_at: summary.started_at,
        completed_at: summary.completed_at,
        problem: summary.problem.map(|problem| ActiveProblem {
            reason: without_unsafe_chars_multiline(&problem.reason),
            ..problem
        }),
        recoverable: not_recoverable_reason.is_none(),
        not_recoverable_reason,
    })
}

/// `GET /control/v1/executions/:execution_id/events?after=<sequence>`.
async fn get_history_events(
    State(state): State<AppState>,
    Path(execution_id): Path<String>,
    Query(query): Query<EventsQuery>,
) -> Result<Json<Vec<Event>>, ApiError> {
    let store = legacy_store(&state, &execution_id)?;
    Ok(Json(read_events(&store, &execution_id, query.after)?))
}

/// `GET /control/v1/executions/:execution_id/timeline`.
async fn get_history_timeline(
    State(state): State<AppState>,
    Path(execution_id): Path<String>,
) -> Result<Json<Timeline>, ApiError> {
    let store = legacy_store(&state, &execution_id)?;
    Ok(Json(read_timeline(&store, &execution_id)?))
}

/// `GET /control/v1/executions/:execution_id/attempts/:attempt_id/conversation`.
async fn get_history_conversation(
    State(state): State<AppState>,
    Path((execution_id, attempt_id)): Path<(String, String)>,
    Query(query): Query<ConversationQuery>,
) -> Result<Json<super::ConversationResponse>, ApiError> {
    let store = legacy_store(&state, &execution_id)?;
    Ok(Json(read_conversation(
        &store,
        &execution_id,
        &attempt_id,
        query.from,
        query.byte_budget,
    )?))
}

/// `GET /control/v1/executions/:execution_id/conductor`.
async fn get_history_conductor(
    State(state): State<AppState>,
    Path(execution_id): Path<String>,
) -> Result<Json<ConductorStateResponse>, ApiError> {
    let store = legacy_store(&state, &execution_id)?;
    Ok(Json(read_conductor_state(&store, &execution_id)?))
}

/// `GET /control/v1/executions/:execution_id/conductor/chat?after=<sequence>`.
async fn get_history_conductor_chat(
    State(state): State<AppState>,
    Path(execution_id): Path<String>,
    Query(query): Query<EventsQuery>,
) -> Result<Json<ConductorChatResponse>, ApiError> {
    let store = legacy_store(&state, &execution_id)?;
    Ok(Json(read_conductor_chat(
        &store,
        &execution_id,
        query.after,
    )?))
}

/// `POST /control/v1/executions/:execution_id/recover` — starts a fresh
/// attempt of a failed legacy execution. One that cannot be recovered is
/// refused with its reason (`409`, `not_recoverable`), not a bare status.
async fn post_history_recover(
    State(state): State<AppState>,
    Path(execution_id): Path<String>,
    Json(request): Json<RecoveryRequest>,
) -> Result<(StatusCode, Json<AcceptedResponse>), ApiError> {
    legacy_store(&state, &execution_id)?;
    let view = history_view(&state, &execution_id)?;
    if let Some(message) = view.not_recoverable_reason {
        return Err(ApiError::Refused {
            code: "not_recoverable",
            message,
        });
    }
    Ok((
        StatusCode::ACCEPTED,
        Json(begin_recovery(&state, execution_id, request)?),
    ))
}
