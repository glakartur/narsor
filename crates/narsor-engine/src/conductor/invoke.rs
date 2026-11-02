//! The harness-backed conductor: one invocation, from the snapshot to a
//! checked answer (U9).
//!
//! # Read-only, per harness (KTD5, R29)
//!
//! The conductor runs on the daemon's default harness, read from
//! [`DefaultHarnessSelector`](crate::persistence::DefaultHarnessSelector)
//! at every invocation, the same way an agent step picks it. A task runner has no selector: its conductor runs on the
//! default its control plane last handed it ([`LiveHarness`], U7) — the
//! harness its task was created with until then — read at every invocation
//! too. It never writes, and how that is enforced depends on the harness:
//!
//! * **claude-code, codex** enforce it themselves: the conductor runs in the
//!   task's workspace with read-only access, which the adapters turn into
//!   `--permission-mode plan` and `--sandbox read-only`;
//! * **opencode** (and any harness this list does not name) has no such
//!   mode, so the conductor runs in an empty scratch directory,
//!   `tasks/{id}/conductor/scratch/` of the work tree, and knows the task
//!   only from its snapshot — which then also carries the workflow's
//!   artifact files, within its budget. On opencode, read-only means
//!   isolation from the workspace, not a sandbox.
//!
//! A workspace that no longer exists is treated like the second case.
//!
//! # One try, no bookkeeping
//!
//! [`HarnessInvoker`] turns one [`Invocation`] into one outcome: it creates
//! no action attempt, emits no `ActionStarted` (the actor records
//! `ConductorStarted`/`Finished`, so D23 never sees an invocation) and
//! writes nothing to the execution. It never retries either — R24's one
//! repeat is the actor's, which counts every try durably before it runs
//! (see [`crate::conductor::actor`]). Every failure — the process, the
//! timeout, an answer without JSON, off the schema, or with a decision the
//! purpose does not allow — is an [`InvocationOutcome::Failed`], and never
//! a reason to invoke the conductor about the failure (R25).
//!
//! # The time limit
//!
//! An invocation is bounded by the configured `agent_timeout`, the limit an
//! agent step without its own is given; with none configured, by
//! [`DEFAULT_CONDUCTOR_TIMEOUT`]. Unlike an agent step it is never
//! unbounded: the label rescue runs inline, under the pass's claim.

use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::conductor::actor::{ConductorAnswer, ConductorInvoker, Invocation, InvocationOutcome};
use crate::conductor::prompt::{answer_schema, conductor_prompt};
use crate::conductor::response::ConductorResponse;
use crate::conductor::snapshot::{render, SnapshotBudget, SnapshotInput};
use crate::conductor::store::ConductorStore;
use crate::config::{agent_args_for, AgentArgs};
use crate::domain::{
    AgentActionConfig, ChoiceDefinition, WorkflowDefinition, WorkflowExecution, WorkspaceAccess,
};
use crate::engine::actions::agent::{
    run_harness_process, DefaultHarnessSource, SpawnEnding, BUILT_IN_HARNESS,
};
use crate::engine::actions::harness::{
    apply_extra_args, HarnessRegistry, HarnessRequest, CLAUDE_CODE, CODEX,
};
use crate::persistence::{ExecutionStore, WorkPaths};

/// The time limit of an invocation when no `agent_timeout` is configured.
pub const DEFAULT_CONDUCTOR_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// The harnesses that enforce read-only access themselves (KTD5).
const SANDBOXED_HARNESSES: &[&str] = &[CLAUDE_CODE, CODEX];

/// How the conductor is kept from writing, for one invocation (KTD5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConductorAccess {
    /// In the task's workspace, which the harness opens read-only.
    ReadOnlyWorkspace,
    /// In an empty scratch directory, knowing only the snapshot.
    Isolated,
}

impl ConductorAccess {
    /// The access the conductor gets on `harness`.
    pub fn for_harness(harness: &str) -> Self {
        if SANDBOXED_HARNESSES.contains(&harness) {
            Self::ReadOnlyWorkspace
        } else {
            Self::Isolated
        }
    }
}

/// The harness a conductor runs on, changeable while it works: the next
/// invocation reads it, the one under way is not touched.
#[derive(Debug)]
pub struct LiveHarness(Mutex<String>);

impl LiveHarness {
    /// A handle holding `harness`.
    pub fn new(harness: impl Into<String>) -> Self {
        Self(Mutex::new(harness.into()))
    }

    /// The harness the next invocation runs on.
    pub fn get(&self) -> String {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Chooses the harness of the invocations to come.
    pub fn set(&self, harness: impl Into<String>) {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = harness.into();
    }
}

/// The runner's live default is also what its agent actions that name no
/// harness start on (U7), read once per action like the daemon's selector.
impl DefaultHarnessSource for LiveHarness {
    fn current_harness(&self) -> String {
        self.get()
    }
}

/// The conductor, run on the default harness (U9).
#[derive(Debug, Clone)]
pub struct HarnessInvoker {
    store: ExecutionStore,
    catalog_dir: PathBuf,
    work: WorkPaths,
    harnesses: HarnessRegistry,
    default_harness: Arc<dyn DefaultHarnessSource>,
    pinned_definition: Option<Arc<WorkflowDefinition>>,
    timeout: Duration,
    budget: SnapshotBudget,
    extra_args: AgentArgs,
}

impl HarnessInvoker {
    /// An invoker over the executions of `store`, whose definitions are
    /// resolved against `catalog_dir` and whose scratch directories live in
    /// the work tree `work_dir`; the stock harnesses, the built-in default
    /// harness, the default limits.
    pub fn new(
        store: ExecutionStore,
        catalog_dir: impl Into<PathBuf>,
        work_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            store,
            catalog_dir: catalog_dir.into(),
            work: WorkPaths::new(work_dir.into()),
            harnesses: HarnessRegistry::defaults(),
            default_harness: Arc::new(BUILT_IN_HARNESS.to_string()),
            pinned_definition: None,
            timeout: DEFAULT_CONDUCTOR_TIMEOUT,
            budget: SnapshotBudget::default(),
            extra_args: AgentArgs::new(),
        }
    }

    /// Returns the invoker adding `args` to every harness run; on a conflict
    /// they replace the arguments the adapter decided on.
    pub fn with_extra_args(mut self, args: AgentArgs) -> Self {
        self.extra_args = args;
        self
    }

    /// Returns the invoker with a registry of its own.
    pub fn with_harnesses(mut self, harnesses: HarnessRegistry) -> Self {
        self.harnesses = harnesses;
        self
    }

    /// Returns the invoker with `source` answering which harness it runs
    /// on, read at every invocation: a fixed name, the daemon's live
    /// selector, or a runner's [`LiveHarness`].
    pub fn with_default_harness_source(mut self, source: Arc<dyn DefaultHarnessSource>) -> Self {
        self.default_harness = source;
        self
    }

    /// Returns the invoker with the one definition every execution of its
    /// store runs: a task runner holds its workflow snapshot, not a
    /// catalogue to resolve names against.
    pub fn with_pinned_definition(mut self, definition: Arc<WorkflowDefinition>) -> Self {
        self.pinned_definition = Some(definition);
        self
    }

    /// Returns the invoker with its time limit derived from the configured
    /// `agent_timeout` (see the module documentation).
    pub fn with_agent_timeout(mut self, agent_timeout_secs: Option<u64>) -> Self {
        self.timeout = agent_timeout_secs.map_or(DEFAULT_CONDUCTOR_TIMEOUT, Duration::from_secs);
        self
    }

    /// Returns the invoker with a time limit of its own.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Returns the invoker with another snapshot budget.
    pub fn with_budget(mut self, budget: SnapshotBudget) -> Self {
        self.budget = budget;
        self
    }

    /// The scratch directory of `execution_id`'s isolated conductor.
    pub fn scratch_dir(&self, execution_id: &str) -> PathBuf {
        self.work
            .task_dir(execution_id)
            .join("conductor")
            .join("scratch")
    }

    /// Runs the conductor once for `invocation`.
    pub async fn invoke_once(&self, invocation: &Invocation) -> InvocationOutcome {
        match self.try_invoke(invocation).await {
            Ok(answer) => InvocationOutcome::Answered(answer),
            Err(error) => InvocationOutcome::Failed { error },
        }
    }

    async fn try_invoke(&self, invocation: &Invocation) -> Result<ConductorAnswer, String> {
        let harness = self.default_harness.current_harness();
        let adapter = self
            .harnesses
            .get(&harness)
            .map_err(|error| error.message)?;

        let execution = self
            .store
            .load_execution(&invocation.execution_id)
            .map_err(|error| format!("the execution could not be read: {error}"))?;

        let workspace = Path::new(&execution.workspace.path);
        let access = match ConductorAccess::for_harness(&harness) {
            ConductorAccess::ReadOnlyWorkspace if workspace.is_dir() => {
                ConductorAccess::ReadOnlyWorkspace
            }
            _ => ConductorAccess::Isolated,
        };
        let working_dir = match access {
            ConductorAccess::ReadOnlyWorkspace => workspace.to_path_buf(),
            ConductorAccess::Isolated => self.empty_scratch_dir(&execution.id)?,
        };

        let mut input = SnapshotInput::collect(
            &self.store,
            &ConductorStore::new(self.store.paths(), &execution.id),
            &execution,
            self.definition(&execution),
            &self.budget,
            access == ConductorAccess::Isolated,
        );
        input.request = Some(invocation.request.clone());
        input.chat_message = invocation.chat_message.clone();
        let snapshot = render(&input, &self.budget);
        let prompt = conductor_prompt(invocation.request.purpose, access, &snapshot.text);

        let config = conductor_config(&prompt);
        let schema = answer_schema(invocation.request.purpose);
        let mut command = adapter
            .invocation(&HarnessRequest {
                config: &config,
                workspace_access: Some(WorkspaceAccess::ReadOnly),
                prompt: &prompt,
                workspace: &working_dir,
                resume_session: None,
                output_schema: Some(&schema),
            })
            .map_err(|error| error.message)?;
        apply_extra_args(
            &mut command,
            &prompt,
            agent_args_for(&self.extra_args, &harness),
        );

        let ending = run_harness_process(
            &command.program,
            &command.args,
            &working_dir,
            Some(self.timeout),
            adapter.stream_parser(),
        )
        .await
        .map_err(|error| error.message)?;

        let finished = match ending {
            SpawnEnding::Finished(finished) => finished,
            SpawnEnding::TimedOut => {
                return Err(format!(
                    "the conductor's harness '{harness}' was still running after {}s and was \
                     killed",
                    self.timeout.as_secs_f64()
                ));
            }
        };
        if let Some(failure) = &finished.run.failure {
            return Err(format!(
                "the conductor's harness '{harness}' failed: {failure}"
            ));
        }

        let response = ConductorResponse::read(
            finished.run.structured_output,
            &finished.run.text,
            invocation.request.purpose,
        )
        .map_err(|error| error.to_string())?;
        Ok(ConductorAnswer {
            message: response.message.clone(),
            response: response.to_value(),
        })
    }

    /// The pinned definition's YAML and the outputs of the current state.
    fn definition(
        &self,
        execution: &WorkflowExecution,
    ) -> Result<(String, Vec<ChoiceDefinition>), String> {
        if let Some(definition) = &self.pinned_definition {
            return Ok(pinned_source(definition, execution));
        }
        let resolved =
            crate::tasks::resolve_definition(&self.catalog_dir, self.store.paths(), execution)
                .map_err(|error| error.to_string())?;
        let outputs = resolved
            .load()
            .ok()
            .and_then(|definition| {
                let state = execution.current_state.as_ref()?;
                definition
                    .states
                    .get(state)
                    .map(|state| state.choices.clone())
            })
            .unwrap_or_default();
        Ok((resolved.source, outputs))
    }

    /// The scratch directory of `execution_id`, emptied: an isolated
    /// conductor starts from nothing every time.
    fn empty_scratch_dir(&self, execution_id: &str) -> Result<PathBuf, String> {
        let dir = self.scratch_dir(execution_id);
        match fs::remove_dir_all(&dir) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "the conductor's scratch directory {} could not be emptied: {error}",
                    dir.display()
                ))
            }
        }
        fs::create_dir_all(&dir).map_err(|error| {
            format!(
                "the conductor's scratch directory {} could not be created: {error}",
                dir.display()
            )
        })?;
        Ok(dir)
    }
}

impl ConductorInvoker for HarnessInvoker {
    fn invoke<'a>(
        &'a self,
        invocation: &'a Invocation,
    ) -> Pin<Box<dyn Future<Output = InvocationOutcome> + Send + 'a>> {
        Box::pin(self.invoke_once(invocation))
    }
}

/// The pinned `definition` as text for the snapshot, and the outputs of the
/// state `execution` is in.
fn pinned_source(
    definition: &WorkflowDefinition,
    execution: &WorkflowExecution,
) -> (String, Vec<ChoiceDefinition>) {
    let source = serde_yaml::to_string(definition)
        .or_else(|_| serde_json::to_string_pretty(definition))
        .unwrap_or_default();
    let outputs = execution
        .current_state
        .as_ref()
        .and_then(|state| definition.states.get(state))
        .map(|state| state.choices.clone())
        .unwrap_or_default();
    (source, outputs)
}

/// The action configuration the adapters build the conductor's command line
/// from: no model, no parameters, read-only.
fn conductor_config(prompt: &str) -> AgentActionConfig {
    AgentActionConfig {
        harness: None,
        permission_grants: Vec::new(),
        model: None,
        workspace_access: Some(WorkspaceAccess::ReadOnly),
        prompt: prompt.to_string(),
        workspace: None,
        parameters: Default::default(),
        read_only_paths: Vec::new(),
        // The conductor is not a step: it has no earlier attempts to show.
        history: false,
    }
}
